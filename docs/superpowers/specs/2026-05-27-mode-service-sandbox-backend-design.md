# shim-k8s: `mode: service` via agent-sandbox CRD

Date: 2026-05-27
Status: Draft (pending review)
Branch: `feature/service-backend` (cut from `feature/voidbox-0.2-migration` so this work builds on the not-yet-merged Job backend; rebase to `main` once PR #1 lands).
Depends on: `docs/superpowers/specs/2026-05-26-voidbox-schema-migration-design.md` (the Job backend).

## Problem

The current `void-shim-k8s` only renders `batch/v1 Job`. That is correct for
`kind: workflow` and `kind: agent, mode: task` — run-to-completion workloads.

It is **not** the right primitive for voidbox `kind: agent, mode: service`,
where the agent runs inside the `voidbox serve` daemon as a long-lived
addressable service. Today the shim refuses such specs only implicitly (the
generated Job would start the daemon, never reach a terminal state, and have
no way for a caller to send the agent messages or fetch telemetry).

To support `mode: service` end-to-end we need:
- A long-running pod with stable identity (DNS hostname) so callers can reach it
- A way to inject the spec into the daemon at startup (the daemon starts empty)
- A way for a caller to send `POST /v1/runs/{id}/messages`, query telemetry,
  and cancel — securely (bearer-token auth, voidbox 0.2.x defaults)
- Lifecycle operations (suspend/resume) at least for "free up resources" semantics

We adopt the `agents.x-k8s.io/v1beta1 Sandbox` CRD (kubernetes-sigs/agent-sandbox)
as the rendering target. Trade-offs vs vanilla `StatefulSet + Service` are
documented in [Alternatives considered](#alternatives-considered); the decision
to take the agent-sandbox dependency is intentional, betting on the SIGs ecosystem.

## Goals

1. Add a second rendering backend to `shim-k8s` that emits a `Sandbox` CR (plus
   two auxiliary `Secret`s for the bearer token and the spec) for specs with
   `kind: agent, mode: service`.
2. Add CLI surface for client-side operations against the live daemon:
   `send`, `cancel`, `telemetry`, `endpoint`, `suspend`, `resume`.
3. Auto-detect the backend from the spec; allow explicit override with
   `--backend job|sandbox|auto`.
4. Ship a working end-to-end smoke against a local kind cluster with
   agent-sandbox controller installed (with graceful degradation if the
   controller install fails — same pattern as the existing image-availability
   check in `kind_smoke.sh`).
5. **The smoke MUST exercise a real LLM round-trip** (not mock mode): send a
   message, get a response from the actual Anthropic API, assert that the
   telemetry reports `tokens_out > 0`. The smoke takes the operator's
   `ANTHROPIC_API_KEY` from env, propagates it to the pod via a Secret,
   degrades gracefully (skip live LLM step) if the env var is absent.

## Non-goals

- `kind: agent, mode: interactive` and `kind: sandbox` (bare VM). Both are
  long-running and could share the same backend, but are out of scope for
  this PR. Tracked as follow-up #1.
- State persistence across suspend/resume. MVP behavior is **nothing
  survives**: suspend tears the pod down, resume starts fresh with the same
  Sandbox CR but a new voidbox `run_id` inside the daemon. voidbox 0.2.x does
  not support snapshot/restore of an in-flight KVM VM; implementing real
  persistence requires upstream void-box changes. Tracked as follow-up #2.
- Ingress / TLS / external exposure. The daemon endpoint is cluster-internal;
  out-of-cluster clients use `kubectl port-forward` (handled transparently by
  the shim). Tracked as follow-up #3.
- Removing the dependency on the agent-sandbox controller. The shim refuses
  to render Sandbox CRs if the CRD is absent (pre-flight check with install
  hint). Cluster operators are responsible for installing the controller.
- Multi-replica services. Sandbox CRD allows `replicas: 0|1` only and we
  always emit `replicas: 1`. Scale-out is not a voidbox concept.

## Decisions and rationale

### Choose `agents.x-k8s.io/v1beta1 Sandbox` over vanilla `StatefulSet+Service`

A vanilla path exists and would deliver the same MVP functionality with
**less operational complexity** (no extra CRD, no controller, no API
versioning risk). We choose Sandbox because:

- It is the de-facto kubernetes-SIGs primitive for agent workloads; future
  ecosystem tooling (Kueue integration, HPA over sandbox pools, dashboards)
  will target this CRD. Building on it gives us those integrations for free
  later, without rewriting the shim.
- It models "singleton, stateful, addressable" as first-class types
  (`replicas` constrained to `[0,1]`, conditions tailored to the workload
  class: `Ready`, `Suspended`, `Finished`).
- It bundles concerns that we would otherwise reimplement piecemeal:
  `lifecycle.expirationSeconds`, automatic Service creation
  (`spec.service: true`), warm-pool adoption.

Trade-offs accepted:
- Cluster operators must install the agent-sandbox controller. Pre-flight
  check fails loud with install hint.
- API is `v1beta1` and may break. We pin to a specific version, bump
  explicitly when it changes.

See [Alternatives considered](#alternatives-considered) for a longer
write-up of the vanilla-k8s path.

### Auto-detect backend from spec

The spec is unambiguous: `kind: agent, mode: service` → `sandbox` backend;
everything else → `job` backend. Auto-detect by default keeps UX uniform:

```bash
void-shim-k8s run --file my-spec.yaml   # backend chosen automatically
```

`--backend job|sandbox|auto` exists for override. Rendering a `mode: service`
spec with `--backend job` emits a warning (not fatal) — the resulting Job
will run the daemon but no client routing exists, which is almost always
wrong. `--backend sandbox` over a non-service spec is rejected outright.

### LLM credential propagation

Provider-to-env-var mapping (default):

| `llm.provider` | Env var the pod expects | Source on operator's machine |
|---|---|---|
| `claude` | `ANTHROPIC_API_KEY` | `$ANTHROPIC_API_KEY` |
| `codex` | `OPENAI_API_KEY` | `$OPENAI_API_KEY` |
| `ollama`, `lm-studio` | (none) | — |
| anything else | — | shim emits warning, no Secret created |

For providers that need a key, the shim auto-creates a Secret
`<sandbox-name>-llm-creds` containing the relevant env var. The pod env
wires the same name into the container. The pod env wires
the same var into the container so voidbox finds it. If the env var is
absent at render time AND `llm.provider` requires one, the shim:

- For `run`: warns once and proceeds (the pod will fail at first LLM call;
  user gets a clear error in `logs`)
- For `render`: emits the Secret stub with empty value and warns; user can
  edit and reapply

Override: `--llm-api-key-env` to name a different env var (e.g.
`CLAUDE_API_KEY`), or `--llm-api-key-secret <name>:<key>` to reference an
existing in-cluster Secret instead of creating one. Mutually exclusive flags.

Lifecycle: the Secret is owned by the Sandbox CR (via `ownerReferences`)
so `rm` cascades and cleans it up.

### Auto-generated bearer token, cluster-internal endpoint

voidbox 0.2.x defaults the daemon to AF_UNIX socket; TCP requires a bearer
token. We auto-generate 32 bytes (64 hex chars), store in a `Secret` named
`<sandbox-name>-token`, mount as env var into the pod. The shim reads the
same Secret to construct client requests.

The endpoint is the agent-sandbox-managed Service (`<name>.<ns>.svc.cluster.local:43100`).
Out-of-cluster clients use `kubectl port-forward sandbox/<name> 0:43100`
transparently — the shim spawns the port-forward, parses the local port,
makes HTTP requests, and reaps the child on `Drop`.

### Container image is shared between backends; entrypoint script branches

The image (`ghcr.io/the-void-ia/void-box:latest` once published, or
`void-box:smoke` for local testing) has a small entrypoint script. The shim
passes `args: ["run"]` for Job backend or `args: ["serve-and-load"]` for
Sandbox. The script handles:

- `run` → `exec voidbox run --file /spec/run.yaml` (current Job behavior)
- `serve-and-load` → start daemon in background, wait for `/v1/runs` to
  respond, POST `/spec/run.yaml`, `wait` on the daemon

This avoids two images for two backends and keeps the image build pipeline
shared.

### Suspend / resume via scale subresource (NOT `lifecycle.suspended`)

The `agents.x-k8s.io/v1beta1 Sandbox` CRD does NOT have a
`spec.lifecycle.suspended` field (an earlier version of this design was
wrong about this). The actual mechanism is `spec.replicas` with
`+kubebuilder:validation:Minimum=0,Maximum=1`, exposed via the
`subresource:scale` annotation:

- `suspend` → `kubectl scale sandbox/<name> --replicas=0`
- `resume`  → `kubectl scale sandbox/<name> --replicas=1`

The controller observes `replicas == 0` and sets the `Suspended` status
condition. The shim's `suspend`/`resume` subcommands wrap these scale
operations.

### Expiration via `lifecycle.shutdownTime` (NOT `expirationSeconds`)

The Lifecycle struct only has `shutdownTime` (absolute RFC3339 timestamp)
and `shutdownPolicy` (`Delete`|`Retain`, default `Retain`). To honor the
spec's `agent.timeout_secs`:

```yaml
lifecycle:
  shutdownTime: "2026-05-27T20:00:00Z"   # render-time: now() + agent.timeout_secs
  shutdownPolicy: Delete                  # we want the Sandbox CR gone, not just the pod
```

Computed at render time on the operator's clock. Skew of a few seconds is
acceptable for this use case.

### MVP: nothing survives suspend

voidbox 0.2.x cannot snapshot a running KVM VM (only Apple VZ can; not
applicable on Linux). Real persistence requires upstream work. We accept
the simpler semantic: `suspend` terminates the pod; `resume` starts a fresh
pod. The Sandbox CR identity (`<name>.<ns>.svc.cluster.local`) persists,
which is enough for "free up resources for now, I'll come back later but
don't care about exact conversational state".

Documented loudly in CLI help and README so users don't expect
StatefulSet-like persistence.

## Architecture

```
shim-core (Cargo workspace member)
├── re-exports void_box::spec::* (unchanged)
├── Backend enum (NEW)
├── detect_backend(spec) -> Backend (NEW)
└── ShimError +5 variants (NEW: MissingCrd, DaemonHttp, PortForward,
                             BackendMismatch, DaemonRunNotFound)

shim-k8s
├── main.rs
│   ├── existing Job subcommands (render/run/status/logs/rm)
│   ├── --backend flag on render/run (auto|job|sandbox)
│   ├── status/logs/rm auto-detect backend by querying both Job + Sandbox
│   └── NEW subcommands: send, cancel, telemetry, endpoint, suspend, resume
├── render_job.rs    (existing logic, factored out of main.rs)
├── render_sandbox.rs (NEW: emits Sandbox CR + 2 Secrets as multi-doc YAML)
├── client.rs        (NEW: reqwest blocking, builds HTTP requests against daemon)
├── portforward.rs   (NEW: kubectl port-forward wrapper with Drop cleanup)
└── crd_check.rs     (NEW: pre-flight `kubectl get crd sandboxes.agents.x-k8s.io`)

Container image (build script lives in repo, currently /tmp/voidbox-smoke):
└── /usr/local/bin/voidbox-entrypoint.sh (NEW: dispatcher for run vs serve-and-load)
```

**Module boundaries:**
- `shim-core` knows voidbox spec + backend taxonomy. Knows nothing about k8s or HTTP.
- `render_*.rs` are pure: `&RunSpec` + opts → YAML `String`. No I/O.
- `client.rs` does HTTP only. Takes an `Endpoint` + token, returns parsed JSON.
- `portforward.rs` does kubectl spawn only. Returns an `Endpoint` (URL string) + a `Drop`-cleanup handle.
- `main.rs` orchestrates: parses CLI, calls renderers, applies via kubectl, calls clients.

## Schema accepted

Same as PR #1: the void-box 0.2.x `RunSpec` verbatim, no shim-specific
extensions. The shim consumes `spec.kind`, `spec.agent.mode`,
`spec.sandbox.{mode,mounts,memory_mb,vcpus,network}`, `spec.name`. For
`kind: agent, mode: service`, the shim additionally requires
`spec.agent.prompt` (validated by voidbox already) and respects
`spec.agent.timeout_secs` (mapped to `lifecycle.expirationSeconds` on the
Sandbox CR).

## CLI surface (additions)

```
void-shim-k8s render --file <spec> [--backend auto|job|sandbox]
void-shim-k8s run    --file <spec> [--backend auto|job|sandbox]
void-shim-k8s status <run_ref>             # auto-detect by querying both Job + Sandbox
void-shim-k8s logs   <run_ref> [--follow]
void-shim-k8s rm     <run_ref> [--force]

# Sandbox-only:
void-shim-k8s send       <run_ref> --message <text> | --message-file <path>
void-shim-k8s cancel     <run_ref>
void-shim-k8s telemetry  <run_ref> [--follow]
void-shim-k8s endpoint   <run_ref>          # prints URL + token + sample curl
void-shim-k8s suspend    <run_ref>          # kubectl patch sandbox/<name> ...
void-shim-k8s resume     <run_ref>          # kubectl patch sandbox/<name> ...

# Global:
-v, --verbose                                # log HTTP requests + kubectl invocations + auto-detect decisions
--endpoint-mode in-cluster|port-forward|auto # default auto
--timeout-secs <N>                           # HTTP timeout, default 30
```

`run_ref` format unchanged in shape: `namespace/<job_or_sandbox_name>`. The
referenced resource determines the backend.

## Rendered output (Sandbox case)

For a spec `examples/run-service.yaml`:

```yaml
api_version: v1
kind: agent
name: my-agent
sandbox: { mode: mock, memory_mb: 1024, vcpus: 2 }
llm:
  provider: claude
  model: claude-haiku-4-5-20251001   # cheapest current Haiku — verify at impl time
agent:
  prompt: "You are an echo agent. Reply to every message verbatim."
  mode: service
  timeout_secs: 3600
```

The shim emits (single `kubectl apply -f -`, 3 or 4 YAML docs — the LLM
creds Secret is omitted when the spec has no `llm` block or when
`--llm-api-key-secret` references an existing Secret):

```yaml
# Doc 1: Sandbox CR
apiVersion: agents.x-k8s.io/v1beta1
kind: Sandbox
metadata:
  name: my-agent-svc-<8hex>
  namespace: default
  labels:
    app: void-shim-k8s
    void.run/id: "<uuid>"
    void.run/backend: "sandbox"
spec:
  replicas: 1
  service: true
  podTemplate:
    metadata:
      labels:
        app: void-shim-k8s
        void.run/id: "<uuid>"
    spec:
      restartPolicy: Never
      containers:
        - name: void-box
          image: ghcr.io/the-void-ia/void-box:latest
          imagePullPolicy: IfNotPresent
          # securityContext.privileged: true  -- only when sandbox.mode in {auto, kvm}
          command: ["/usr/local/bin/voidbox-entrypoint.sh"]
          args: ["serve-and-load"]
          env:
            - name: VOIDBOX_DAEMON_TOKEN
              valueFrom:
                secretKeyRef:
                  name: my-agent-svc-<8hex>-token
                  key: token
            - name: ANTHROPIC_API_KEY    # only when llm.provider needs it
              valueFrom:
                secretKeyRef:
                  name: my-agent-svc-<8hex>-llm-creds
                  key: ANTHROPIC_API_KEY
          ports:
            - { name: daemon, containerPort: 43100, protocol: TCP }
          readinessProbe:
            httpGet:
              path: /v1/runs
              port: daemon
              httpHeaders:
                - { name: Authorization, value: "Bearer $(VOIDBOX_DAEMON_TOKEN)" }
            initialDelaySeconds: 2
            periodSeconds: 2
            timeoutSeconds: 2
            failureThreshold: 30
          volumeMounts:
            - { name: spec, mountPath: /spec, readOnly: true }
            # devkvm + mount-N when applicable (same rules as Job backend)
      volumes:
        - { name: spec, secret: { secretName: my-agent-svc-<8hex>-spec } }
  # lifecycle.shutdownTime computed as now() + agent.timeout_secs (RFC3339)
  # lifecycle.shutdownPolicy: Delete
  lifecycle:
    shutdownTime: "2026-05-27T20:00:00Z"
    shutdownPolicy: Delete

---
# Doc 2 (only when LLM provider needs an api key): Secret with credentials
apiVersion: v1
kind: Secret
metadata:
  name: my-agent-svc-<8hex>-llm-creds
  namespace: default
  ownerReferences:
    - { apiVersion: agents.x-k8s.io/v1beta1, kind: Sandbox, name: my-agent-svc-<8hex>, uid: <bound at apply-time via separate kubectl step> }
type: Opaque
data:
  ANTHROPIC_API_KEY: <base64(operator-env)>

---
# Doc 3: Secret with bearer token
apiVersion: v1
kind: Secret
metadata:
  name: my-agent-svc-<8hex>-token
  namespace: default
  labels: { app: void-shim-k8s, void.run/id: "<uuid>" }
type: Opaque
data:
  token: <base64(64hex)>

---
# Doc 4: Secret with the run spec
apiVersion: v1
kind: Secret
metadata:
  name: my-agent-svc-<8hex>-spec
  namespace: default
  labels: { app: void-shim-k8s, void.run/id: "<uuid>" }
type: Opaque
data:
  run.yaml: <base64(yaml)>
```

## Container entrypoint

Baked into the image at `/usr/local/bin/voidbox-entrypoint.sh`:

```bash
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
    for i in $(seq 1 30); do
      if curl -sf -H "Authorization: Bearer ${VOIDBOX_DAEMON_TOKEN}" \
              http://127.0.0.1:43100/v1/runs >/dev/null 2>&1; then
        break
      fi
      sleep 0.5
    done
    # POST /v1/runs takes JSON {file, run_id?, input?, policy?, snapshot?}
    # The daemon reads the spec from `file` (filesystem path inside the pod).
    curl -fsS -X POST \
      -H "Authorization: Bearer ${VOIDBOX_DAEMON_TOKEN}" \
      -H "Content-Type: application/json" \
      -d '{"file":"/spec/run.yaml"}' \
      http://127.0.0.1:43100/v1/runs
    wait "$DAEMON_PID"
    ;;
  *)
    echo "unknown mode: $MODE" >&2
    exit 2
    ;;
esac
```

Verifications during implementation:
- Confirm `voidbox serve --token-file <path>` is the actual flag name (per
  CHANGELOG snippet about TCP listener); adjust if upstream uses a different
  spelling.
- Confirm `POST /v1/runs` accepts `application/yaml`. If JSON-only, the
  entrypoint converts with `yq -o json`.

## Client (HTTP)

Crate addition: `reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }`.

Endpoint resolution:
```rust
fn resolve_endpoint(ns: &str, sandbox: &str, mode: EndpointMode) -> Result<(Endpoint, Option<PortForward>), ShimError>
```
- `EndpointMode::InCluster` → returns DNS endpoint directly.
- `EndpointMode::PortForward` → spawns `kubectl port-forward -n <ns> sandbox/<name> 0:43100`, parses the "Forwarding from 127.0.0.1:<port> -> 43100" line, returns `(http://127.0.0.1:<port>, Some(PortForward(child)))`.
- `EndpointMode::Auto` → InCluster if `KUBERNETES_SERVICE_HOST` is set, else PortForward.

`PortForward::drop` kills the child process. Each subcommand creates its own port-forward and tears it down on return (or on `?` early-exit via Drop).

Token resolution:
```rust
fn read_token(ns: &str, sandbox: &str) -> Result<String, ShimError>
// kubectl get secret <name>-token -n <ns> -o jsonpath='{.data.token}' | base64 -d
```

Voidbox-internal `run_id` resolution:
```rust
fn first_run_id(endpoint: &Endpoint, token: &str) -> Result<String, ShimError>
// GET /v1/runs, parse JSON, return items[0].id, error if !=1
```
We assume one Sandbox CR → one daemon → one run inside the daemon (the
spec POSTed by the entrypoint). If a future version of voidbox supports
multi-run daemons, we'd add `--run-id` flag to disambiguate. For now,
`DaemonRunNotFound(n)` if `items.len() != 1`.

HTTP operations:
```rust
fn send_message(ep: &Endpoint, token: &str, run_id: &str, body: &str) -> Result<MessageResponse, ShimError>
fn cancel(ep: &Endpoint, token: &str, run_id: &str) -> Result<(), ShimError>
fn telemetry(ep: &Endpoint, token: &str, run_id: &str) -> Result<TelemetrySnapshot, ShimError>
```

Body shape for `send_message` (verified against `AppendMessageRequest` in
`/home/diego/github/agent-infra/void-box/src/daemon.rs:113`):

```json
{"role": "user", "content": "<text>"}
```

The shim defaults `role` to `"user"` (callers can override with
`--role assistant|system|user` if needed).

## Error handling

| Situation | Variant | Hint included in message |
|---|---|---|
| Spec is `mode: service` + `--backend job` | (warning, not fatal) | "service routing won't work; remove --backend or use sandbox" |
| `--backend sandbox` over non-service spec | `BackendMismatch` (fatal) | "spec is not kind:agent,mode:service" |
| Pre-flight: CRD absent | `MissingCrd("sandboxes.agents.x-k8s.io")` | `kubectl apply -k github.com/kubernetes-sigs/agent-sandbox/config/default` |
| `send`/`cancel`/`telemetry` on a Job run_ref | `BackendMismatch` | "this op requires sandbox backend" |
| Sandbox exists but pod not Ready | `CommandFailed` | "retry in a few seconds; pod status: <phase>" |
| Daemon 401 | `DaemonHttp("401: token mismatch — Secret may have been recreated")` | inspect `kubectl get secret <name>-token` |
| Daemon 404 on `/runs/<id>/messages` | `DaemonRunNotFound(0)` | "run may have completed or been cancelled" |
| `kubectl port-forward` non-zero exit | `PortForward("<stderr>")` | "check kubectl context" |
| `kubectl port-forward` hangs (no port line in 10s) | `PortForward("timed out waiting for bind")` | "retry; may be transient" |
| Token Secret missing | `CommandFailed` | "Sandbox may have been deleted out-of-band" |

No silent fallback: token, endpoint, CRD checks all fail loud rather than
degrade. `--verbose` reveals decision points.

## Testing

### Unit tests (no cluster, no network)

`shim-core`:
- `detect_backend` table tests covering 5 cases: agent+task, agent+service,
  agent+interactive (→ Job for now, will be Sandbox in follow-up #1),
  workflow, sandbox(kind), pipeline.

`shim-k8s::render_sandbox`:
- `render_sandbox_includes_token_secret_ref` — env var + secretKeyRef present
- `render_sandbox_includes_readiness_probe` — probe targets `:43100/v1/runs` with bearer header
- `render_sandbox_emits_service_true` — `spec.service: true`
- `render_sandbox_includes_kvm_for_mode_kvm` — privileged + devkvm volume (parallel to Job test)
- `render_sandbox_emits_three_docs` — Sandbox + 2 Secrets, separated by `---`
- `render_sandbox_token_secret_data_is_base64_64hex` — generated token is base64-encoded 64 hex chars
- `render_sandbox_spec_secret_round_trip` — base64-decoded content matches `serde_yaml::to_string(&spec)`
- `render_sandbox_sets_expiration_from_timeout_secs` — `lifecycle.expirationSeconds` mirrors `agent.timeout_secs` when set, absent when not

`shim-k8s::client`:
- `build_messages_url` — `<base>/v1/runs/<id>/messages`
- `build_auth_header` — `Authorization: Bearer <token>`
- `parse_port_forward_output` — extracts port from "Forwarding from 127.0.0.1:<port> -> 43100"
- `send_message_posts_correct_body` — wiremock server, assert POST body
- `cancel_posts_empty_body` — wiremock
- `telemetry_parses_json` — wiremock with canned response
- `first_run_id_errors_when_zero_or_many` — wiremock returning [] and [a,b]

`shim-k8s::main`:
- `backend_mismatch_warns_not_fatal` — `--backend job` on service spec exits 0 but writes warning to stderr
- `backend_mismatch_fatal_in_other_direction` — `--backend sandbox` on workflow spec exits non-zero
- `auto_detect_picks_sandbox_for_mode_service` — render call routes to render_sandbox
- `auto_detect_picks_job_for_mode_task` — render call routes to render_job

`shim-k8s::portforward`:
- `drop_kills_child_process` — spawn a sleep child, drop the handle, assert PID gone

### Render-only path

```bash
cargo run -p shim-k8s -- render --file examples/run-service.yaml > /tmp/sb.yaml
python3 -c "import yaml; docs=list(yaml.safe_load_all(open('/tmp/sb.yaml'))); assert len(docs)==3"
```

### Live LLM round-trip

The MVP smoke MUST verify that a real Anthropic API call works end-to-end
through the shim. Steps:

```bash
# Operator sets ANTHROPIC_API_KEY in env before running smoke
export ANTHROPIC_API_KEY=sk-ant-...

# Shim picks it up, creates Secret in cluster, wires into pod
REF=$(target/release/void-shim-k8s run --file examples/run-service.yaml \
  --image void-box:smoke --image-pull-policy Never)

# Wait Ready, then send a message that elicits a real response
target/release/void-shim-k8s send "$REF" --message "Reply with exactly: 'pong'"

# Telemetry should now report tokens_out > 0 (real LLM call happened)
TEL=$(target/release/void-shim-k8s telemetry "$REF")
TOKENS_OUT=$(echo "$TEL" | jq -r '.tokens.out // 0')
test "$TOKENS_OUT" -gt 0 || { echo "FAIL: no LLM tokens consumed; daemon may be in mock mode or API key wrong"; exit 1; }

target/release/void-shim-k8s cancel "$REF"
target/release/void-shim-k8s rm "$REF"
```

Graceful degradation:
- If `ANTHROPIC_API_KEY` is unset, the smoke skips the LLM-round-trip
  assertion and logs `LLM smoke skipped: set ANTHROPIC_API_KEY to enable`.
  The render + apply + Sandbox-Ready path still runs (validates the
  rendering and the controller integration). The CI workflow (future
  follow-up) should run with the key set as a GitHub Actions secret.
- If the API call fails for reasons other than missing key (rate limit,
  network), the smoke fails loud — that's a real bug to investigate.

Cost: each smoke run consumes ~50-200 input tokens + ~5-20 output tokens
against the cheapest available model. Negligible (`<$0.01/run`). The
example spec pins a specific cheap model via `llm.model:
claude-3-5-haiku-latest` (or whatever the cheapest current model is —
verify at implementation time).

### Integration smoke (`scripts/kind_smoke.sh` extended)

```bash
# After the existing Job smoke succeeds:

if kubectl apply -k github.com/kubernetes-sigs/agent-sandbox/config/default 2>/dev/null; then
  kubectl wait --for=condition=Available deploy -l app=agent-sandbox -A --timeout=120s

  docker build -t void-box:smoke /tmp/voidbox-smoke  # rebuilt with new entrypoint.sh
  kind load docker-image void-box:smoke --name void-shims-smoke

  REF=$(target/release/void-shim-k8s run --file examples/run-service.yaml \
    --image void-box:smoke --image-pull-policy Never)
  echo "service run_ref: $REF"

  # Wait for Sandbox Ready
  for i in $(seq 1 60); do
    READY=$(kubectl get sandbox ${REF##*/} -n ${REF%%/*} \
      -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "")
    [[ "$READY" == "True" ]] && break
    sleep 2
  done

  target/release/void-shim-k8s telemetry $REF
  target/release/void-shim-k8s send $REF --message "hello"
  target/release/void-shim-k8s cancel $REF
  target/release/void-shim-k8s rm $REF
  log "service smoke OK"
else
  log "agent-sandbox controller install failed; skipping service-path smoke (this is allowed)"
fi
```

If controller install fails (network, permissions, etc.), the script logs
and continues with exit 0. Only failure of the steps after a successful
install is treated as a test failure.

### Out of scope (manual testing only)

- Suspend/resume with a real conversational agent (verifying "nothing
  survives" behaves as documented)
- Behavior under cluster-wide eviction, node OOM
- Comparison vs vanilla StatefulSet+Service performance/operability
- LLM tokens/cost regression tracking over time

## Alternatives considered

### Vanilla `StatefulSet + headless Service + Secret`

Functionally equivalent for the MVP we are building:

| Capability | Vanilla path | agent-sandbox path |
|---|---|---|
| Stable identity | StatefulSet ordinal + headless Service | Sandbox + auto Service |
| Suspend / Resume | `kubectl scale sts/<name> --replicas=0/1` | `kubectl patch sandbox/.spec.lifecycle.suspended` |
| PVC | `volumeClaimTemplates` on StatefulSet | `volumeClaimTemplates` on Sandbox |
| Token Secret + readiness probe | Same | Same |
| Conditions for status | Pod phase interpretation | Tailored Sandbox conditions |
| Expiration | DIY (CronJob or ttl) | `lifecycle.expirationSeconds` built-in |
| Ecosystem (Kueue, etc.) | Outside | Inside |

Vanilla path wins on operational simplicity (no extra controller, no CRD
versioning); agent-sandbox wins on ecosystem alignment and on first-class
semantic types for the workload class.

Decision: take the agent-sandbox dependency now, with a documented fallback
plan: if the controller proves operationally burdensome in practice, the
rendering layer is small enough (~200 LOC) to swap to vanilla
StatefulSet+Service without changing the CLI surface, the client, or the
container image.

### Two binaries (`void-shim-k8s` for Job, `void-shim-k8s-svc` for Sandbox)

Cleaner separation but doubles maintenance: two binaries, two build
targets, two READMEs. Auto-detect in a single binary is unambiguous (the
spec literally says `mode: service` or not), so the separation buys
nothing. Rejected.

### Init container that POSTs the spec instead of bundled entrypoint script

Two containers per pod, ordering dependency. The entrypoint-script approach
keeps the pod single-container and treats the daemon process as the unit of
liveness. Rejected.

### Use `kube` crate for typed Sandbox CR construction

Would replace `format!`-based YAML emission with serde-typed structs.
Pulls a sizable dependency (`kube-runtime` + `k8s-openapi`), requires
generated/hand-written types for the agent-sandbox CRD (no published Rust
bindings). Current `format!` approach is the same pattern used for the Job
backend in PR #1; consistent and small. Rejected for this iteration; may
revisit if the renderer outgrows `format!`.

## Future work (deferred to follow-up specs)

1. **`kind: agent, mode: interactive` + `kind: sandbox`** — same backend
   plumbing, slightly different render shape (no daemon for `kind: sandbox`;
   no `agent.prompt` required for `mode: interactive`).
2. **Real persistence across suspend/resume** — requires upstream void-box
   support for snapshot/restore of running KVM VMs, or an application-level
   checkpoint protocol (cancel + save inbox to ConfigMap + restore on
   resume). Either way: separate design.
3. **Ingress / TLS / external exposure** — for use cases where the agent
   serves external traffic. Will involve cert-manager, optional ingress
   controller dependency. Separate design.
4. **Warm pool / fast-resume integration** — agent-sandbox supports warm
   pools natively; we'd render warm-pool annotations to let the controller
   adopt pre-warmed pods. Useful once cold-start latency matters in practice.
5. **CI workflow running both smokes (Job + Sandbox)** — same deferred item
   as in PR #1's spec §11.7.

## Risks and follow-ups

- **`agents.x-k8s.io/v1beta1` is unstable.** Field names and validation
  rules may shift. Mitigation: pin a release tag of the controller in
  install instructions; bump explicitly and re-test on smoke.
- **~~`voidbox serve --token-file` flag~~** — verified: flag exists exactly as
  named in `src/bin/voidbox/main.rs:183` (`token_file: Option<PathBuf>`,
  parsed from `--token-file`).
- **~~`POST /v1/runs` body format~~** — verified: JSON only, struct
  `CreateRunRequest { file, input?, run_id?, policy?, snapshot? }` at
  `src/daemon.rs:68`. The `file` field is a filesystem path; daemon reads
  the spec from disk at that path. Entrypoint script posts
  `{"file":"/spec/run.yaml"}`.
- **~~`POST /v1/runs/{id}/messages` body~~** — verified: JSON,
  `AppendMessageRequest { role, content }` at `src/daemon.rs:113`.
- **Auto-detect of `run_id` inside the daemon** assumes one run per Sandbox.
  Future multi-tenant daemons would break this. Mitigation: add `--run-id`
  flag when the assumption breaks.
- **Path dependency on `void-box` (inherited from PR #1)** still applies.
  Same mitigation: factor `void-box::spec` into a sibling crate, switch to
  `git` dependency.
- **kind/k3d cannot expose `/dev/kvm` to pods by default.** Mock mode
  sidesteps this for the smoke. Real KVM testing requires bare-metal or
  specially-configured nodes.
