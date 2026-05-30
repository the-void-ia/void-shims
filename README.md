# void-shims

Integration shims that let existing orchestrators run [void-box](https://github.com/the-void-ia/void-box) (microVM-isolated agent runtimes) as a first-class workload.

- `shim-k8s`: Kubernetes — auto-selects between `batch/v1 Job` (one-shot) and
  `agents.x-k8s.io/v1alpha1 Sandbox` (long-running services) based on the spec.
- `shim-containerd`: containerd runtime v2 shim (stub).
- `shim-libvirt`: libvirt domain adapter (stub).

## What this gives you

- **Run any void-box `RunSpec` on Kubernetes** with the same YAML schema you use locally.
- **Microvm (KVM) isolation** for each agent — stronger than gVisor / Kata; smaller than full VMs.
- **Native k8s lifecycle** for long-running agents via the
  [`kubernetes-sigs/agent-sandbox`](https://github.com/kubernetes-sigs/agent-sandbox)
  CRD: stable identity, headless `Service`, `suspend` / `resume` via `kubectl scale`,
  `shutdownTime` auto-expiration.
- **Per-agent credential isolation**: `ANTHROPIC_API_KEY` (or `OPENAI_API_KEY` for codex)
  is read from the operator's env at render time, stored in a per-Sandbox
  `Secret`, mounted into the pod, propagated into the guest VM.
- **Built-in observability**: per-second host telemetry + structured run events
  (`RunStarted`, `SpecLoaded`, `RunCompleted`, …) over the daemon's HTTP API.
- **Cost tracking**: tokens-in / tokens-out / USD reported per run.

## Quick start

```bash
# Build
cargo build --workspace --release

# Render a Job manifest (no cluster needed)
./target/release/void-shim-k8s render --file ./examples/run.yaml > /tmp/job.yaml

# Apply against a cluster
./target/release/void-shim-k8s run --file ./examples/run.yaml
# → default/void-run-smoke-test-abc12345

# Lifecycle
./target/release/void-shim-k8s status default/void-run-smoke-test-abc12345
./target/release/void-shim-k8s logs   default/void-run-smoke-test-abc12345 --follow
./target/release/void-shim-k8s rm     default/void-run-smoke-test-abc12345
```

## Backend selection

The shim auto-detects which k8s primitive fits the spec:

| Spec | Backend | When |
|---|---|---|
| `kind: agent, mode: task` (default) | `batch/v1 Job` | One-shot prompt → exit |
| `kind: workflow` | `batch/v1 Job` | Multi-stage pipeline → exit |
| `kind: pipeline` | `batch/v1 Job` | Multi-box pipeline → exit |
| `kind: agent, mode: service` | `agents.x-k8s.io/v1alpha1 Sandbox` | Long-running addressable agent |

Override with `--backend job|sandbox|auto` (default `auto`).

## Service mode (long-running agents)

For `kind: agent, mode: service`, the shim renders a `Sandbox` CR plus three
auxiliary `Secret`s:

- `<name>-token` — bearer token for the daemon's TCP listener
- `<name>-spec` — the RunSpec YAML, mounted at `/spec/run.yaml`
- `<name>-llm-creds` — `ANTHROPIC_API_KEY` (or other LLM creds) when applicable

### Requirements

1. **`agent-sandbox` controller** installed in the cluster:
   ```bash
   kubectl apply -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.4.6/manifest.yaml
   kubectl wait --for=condition=Established crd/sandboxes.agents.x-k8s.io --timeout=60s
   kubectl wait --for=condition=Available deploy --all -n agent-sandbox-system --timeout=180s
   ```

2. **A container image with `voidbox` 0.2.x + entrypoint dispatcher** baked in.
   Build locally:
   ```bash
   ./crates/shim-k8s/dockerfile/build.sh /path/to/voidbox
   # → image void-box:smoke
   ```

3. **For real microVMs (`sandbox.mode: kvm`):** kernel + initramfs baked into
   the image AND the cluster's nodes must expose `/dev/kvm`. The bundled
   `dockerfile/build.sh` ships only the voidbox binary; for KVM mode you need
   a Dockerfile that also COPYs the kernel (e.g. `target/vmlinux-slim-x86_64`)
   and an agent-enabled initramfs (e.g. `target/void-box-claude.cpio.gz`) and
   sets `ENV VOID_BOX_KERNEL=… VOID_BOX_INITRAMFS=…`. See
   [`docs/superpowers/specs/2026-05-27-mode-service-sandbox-backend-design.md`](docs/superpowers/specs/2026-05-27-mode-service-sandbox-backend-design.md)
   for the full pattern.

### Usage

```bash
# Operator-side env is auto-propagated into the per-Sandbox Secret
export ANTHROPIC_API_KEY=sk-ant-...

REF=$(./target/release/void-shim-k8s run --file examples/run-service.yaml)
# → default/smoke-svc-abc12345

# Inspect state
./target/release/void-shim-k8s telemetry $REF
./target/release/void-shim-k8s status $REF

# Conversation (caveat below)
./target/release/void-shim-k8s send $REF --message "hello"

# Lifecycle
./target/release/void-shim-k8s suspend $REF    # kubectl scale --replicas=0
./target/release/void-shim-k8s resume  $REF    # kubectl scale --replicas=1
./target/release/void-shim-k8s cancel  $REF    # POST /v1/runs/<id>/cancel
./target/release/void-shim-k8s rm      $REF    # delete Sandbox CR (cascades)

# Raw connection info (URL + bearer token) for curl / SDK integration
./target/release/void-shim-k8s endpoint $REF
```

**Endpoint mode:** out-of-cluster clients use `kubectl port-forward`
(spawned + reaped by the shim transparently). In-cluster callers use the
DNS `<sandbox>.<ns>.svc.cluster.local:43100`. Override with
`--endpoint-mode auto|in-cluster|port-forward`.

## Local smoke (kind)

End-to-end against a local `kind` cluster:

```bash
# Prereqs: docker running, kind + kubectl on PATH

./scripts/kind_smoke.sh                  # Job-path only (mock sandbox, fast)
./scripts/kind_smoke.sh --keep-cluster   # keep cluster alive for debugging

# With API key, exercises Sandbox path + real LLM round-trip too
export ANTHROPIC_API_KEY=sk-ant-...
./scripts/kind_smoke.sh
```

For real KVM (`sandbox.mode: kvm`), kind needs the host's `/dev/kvm` exposed
to the node container:

```yaml
# /tmp/kind-kvm-config.yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraMounts:
      - hostPath: /dev/kvm
        containerPath: /dev/kvm
```

```bash
kind create cluster --name void-shims-smoke --config=/tmp/kind-kvm-config.yaml
```

## Validated end-to-end (2026-05-27)

Full chain proven against a real Anthropic API call:

| Step | Result |
|---|---|
| Shim renders Sandbox CR + 4 Secrets | ✅ |
| agent-sandbox controller creates pod | ✅ |
| Pod Ready (exec readiness probe with bearer auth) | ✅ 4 s |
| `voidbox serve` accepts spec via `POST /v1/runs` | ✅ |
| MicroVM boots with `/dev/kvm` + claude rootfs | ✅ telemetry CID assigned |
| Claude Code CLI inside VM reads `ANTHROPIC_API_KEY` | ✅ |
| Real Anthropic API call | ✅ 7in / 148out tokens, $0.0303 |
| Agent writes `/workspace/output.json` → published as run output | ✅ 4 bytes ("pong") |
| Telemetry stream | ✅ 21 samples |
| Clean exit (`RunCompleted` event) | ✅ |
| `void-shim-k8s rm` cascades cleanup | ✅ |

## Specs and reference

- [`spec/shim-v0.2.md`](spec/shim-v0.2.md) — shim contract (CLI, runtime model, target requirements)
- [`docs/superpowers/specs/`](docs/superpowers/specs/) — design docs (schema migration, Service-mode Sandbox backend)
- [`docs/superpowers/plans/`](docs/superpowers/plans/) — implementation plans
- [`examples/run.yaml`](examples/run.yaml) — task-mode agent (Job backend)
- [`examples/run-service.yaml`](examples/run-service.yaml) — service-mode agent (Sandbox backend)

## Workspace

```bash
cargo build --workspace
cargo test  --workspace                          # 44 tests (3 shim-core, 41 shim-k8s)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI (`.github/workflows/ci.yml`) runs all four on every push / PR.
Pre-commit hooks (`.pre-commit-config.yaml`) optionally run the same locally.

## Known limitations

- **`send` / `telemetry` / `cancel` over `kubectl port-forward` fails with
  `reqwest`.** The HTTP request errors with "error sending request" though a
  direct `curl` against the same port-forward returns 200. Likely an HTTP/1.1
  vs HTTP/2 / connection-pool interaction. **Workaround:**
  `void-shim-k8s endpoint $REF` to print URL+token, then `curl` directly. The
  full end-to-end execution path (apply → pod Ready → daemon serving → real
  LLM call) is unaffected — only the shim's outbound HTTP from the operator's
  machine. In-cluster callers (`--endpoint-mode in-cluster`) bypass
  port-forward entirely.

- **Suspend / resume is not state-preserving.** void-box 0.2.x cannot snapshot
  a running KVM VM (only Apple VZ on macOS has that API). `suspend` scales the
  Sandbox to 0 replicas; `resume` starts a fresh pod. The Sandbox CR identity
  persists; conversational / VM state does not.

- **`kind` clusters don't expose `/dev/kvm` by default.** For real microVMs in
  kind, use the `extraMounts` config above. Without it, only `sandbox.mode: mock`
  works (the mock path stubs the agent execution — no real LLM call).

- **The released `agent-sandbox` manifest (`v0.4.6`) ships `v1alpha1` only.**
  The void-box source has `v1beta1` types but they're not yet in a release. The
  shim renders `v1alpha1`; when `v1beta1` ships, a single-line bump is needed.

- **Path dependency on `void-box`.** `crates/shim-core/Cargo.toml` references
  `../../../agent-infra/void-box` directly. Cloning `void-shims` requires
  cloning `void-box` at the same relative path. Tracked as follow-up.

## Notes

- Shims are *integrations*, not a replacement for `void-control`.
- Each `voidbox run` invocation already executes one microVM per RunSpec;
  the shim does not enforce an additional isolation profile.

## Next steps

- Debug `reqwest` + `kubectl port-forward` HTTP path — most likely fix:
  swap to `ureq` or shell out to `curl`.
- Persistence across suspend/resume — needs upstream void-box snapshot/restore
  for in-flight KVM VMs.
- `kind: agent, mode: interactive` and `kind: sandbox` (bare VM) — same Sandbox
  backend, slightly different render shape.
- Ingress / TLS for external clients.
- `--image-pull-secret` for private GHCR images.
- GitHub Actions job running `scripts/kind_smoke.sh` (with `ANTHROPIC_API_KEY`
  as a repo secret) for regression coverage of the LLM round-trip.
- Factor `void-box::spec` into a sub-crate so `shim-core` can switch from
  `path` to `git` dependency.
- Migrate to `agents.x-k8s.io/v1beta1` Sandbox when released.
