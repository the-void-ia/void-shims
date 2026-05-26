# Void Shims Specification

Version: v0.2 Status: Draft Scope: Integration shims for running
Void-Box on existing orchestrators (K8s / containerd / libvirt)

------------------------------------------------------------------------

## 1. Goal

A shim is an integration adapter that allows an external orchestrator
(e.g. Kubernetes, containerd, libvirt) to execute Void-Box without
adopting void-control.

Shims must remain thin: - Map a RunSpec to an external primitive (Job /
Task / Domain) - Expose logs and status using the platform's idioms -
Avoid implementing full control-plane features

------------------------------------------------------------------------

## 2. Non-Goals

A shim MUST NOT: - Implement distributed scheduling - Implement durable
desired/observed reconciliation - Orchestrate stages as first-class
resources - Provide multi-run fairness beyond simple limits

Those responsibilities belong to void-control.

------------------------------------------------------------------------

## 3. Runtime Model

void-box 0.2.x runs one microVM per `voidbox run` invocation by construction.
Shims do not enforce an isolation profile — the RunSpec is consumed verbatim
and executed as-is by the void-box binary inside the container/pod/domain
managed by the shim.

------------------------------------------------------------------------

## 4. Inputs and Outputs

### 4.1 Input: RunSpec

Shims accept the void-box `RunSpec` schema verbatim (`api_version: v1`,
`kind: agent|workflow|pipeline|sandbox`, see the void-box repository for
the canonical definition). No shim-specific transformations beyond what is
documented in §7 (target requirements).

### 4.2 Output

Shims must return a RunRef:

-   K8s: namespace/job_name
-   containerd: namespace/task_id
-   libvirt: domain_name

They must expose: - status (Pending / Running / Succeeded / Failed /
Unknown) - exit code when possible - logs (best effort)

------------------------------------------------------------------------

## 5. CLI Contract (Recommended)

void-shim-`<target>`{=html} run --file \<spec.yaml\>
void-shim-`<target>`{=html} status `<run_ref>`{=html}
void-shim-`<target>`{=html} logs `<run_ref>`{=html} \[--follow\]
void-shim-`<target>`{=html} rm `<run_ref>`{=html}
void-shim-`<target>`{=html} render --file \<spec.yaml\>

------------------------------------------------------------------------

## 6. Runtime Invocation Contract

Shims must invoke Void-Box enforcing the shim execution profile.

Example:

    voidbox run --file /spec/run.yaml

No silent fallback for declared mounts. Policy violations must surface
as failures.

------------------------------------------------------------------------

## 7. Target Requirements

### 7.1 void-shim-k8s (MVP Target)

Primitive: - Kubernetes Job

Requirements: - privileged execution if KVM required - mount /dev/kvm if
needed - provide workflow spec via ConfigMap, PVC, or mount

Status mapping: - Job Active -\> Running - Job Succeeded -\> Succeeded -
Job Failed -\> Failed

Auto-derivation:
- `securityContext.privileged` and `/dev/kvm` mount are auto-derived from
  `sandbox.mode` (`auto`/`kvm` → enabled, `mock`/`vz` → disabled). Override
  with `--mount-kvm-override`.
- `sandbox.mounts[]` are rendered as paired `volumes` + `volumeMounts`. The
  default strategy is `hostPath`; override with `--mount-strategy emptyDir`
  for ephemeral pod-local storage (e.g. CI smoke tests).

------------------------------------------------------------------------

### 7.2 void-shim-containerd (Later)

Primitive: - containerd task (runtime v2)

MVP: - Execute void-box run - Return correct exit code - Basic stdout
streaming

------------------------------------------------------------------------

### 7.3 void-shim-libvirt (Later)

Primitive: - libvirt domain

MVP: - Create domain - Start VM - Wait for exit - Capture console output

------------------------------------------------------------------------

## 8. Security Requirements

Shims must not weaken isolation guarantees. Declared mounts and policies
must be enforced strictly.

------------------------------------------------------------------------

## 9. Repository Layout (Mono-repo Recommended)

void-shims/ ├── spec/ │ └── shim-v0.2.md ├── crates/ │ ├── shim-core/ │
├── shim-k8s/ │ ├── shim-containerd/ │ └── shim-libvirt/ ├── scripts/ │ └──
kind_smoke.sh └── README.md

------------------------------------------------------------------------

## 10. Compatibility Note

Shims complement void-control. Use shims to integrate with existing
orchestrators. Use void-control for full agent orchestration
capabilities.

------------------------------------------------------------------------

## 11. Next Steps (Post-MVP)

### 11.1 Registry Auth

`void-shim-k8s` should support private registry pulls (for example GHCR)
through explicit image pull secret wiring.

### 11.2 Runtime Invocation Flexibility

Current invocation assumes a shell entrypoint. Shims should support
shell-less images by allowing explicit command/args execution mode.

### 11.3 Mount Translation

Done in v0.2: `sandbox.mounts[]` is rendered as paired k8s `volumes` +
`volumeMounts` with strict validation (absolute host paths only, no
newlines). `--mount-strategy emptyDir` overrides the default `hostPath`
for ephemeral pod-local storage.

### 11.4 Observability

Status/log APIs should expose actionable failure context (image pull
errors, container termination reason, and exit code when available).

### 11.5 Repeatable Validation

Done in v0.2: `scripts/kind_smoke.sh` exercises render → dry-run apply →
(optionally) run → status loop → logs → rm against a local `kind` cluster.
CI integration tracked separately in §11.7.

### 11.6 agent-sandbox CRD Backend

Add a second renderer for the kubernetes-sigs `agents.x-k8s.io/v1beta1`
`Sandbox` CRD, for specs with `kind: agent, mode: service|interactive` or
`kind: sandbox` where long-running pods with suspend/resume semantics are
the right model. Selector flag: `--backend job|sandbox|auto`. Tracked in a
separate design doc.

### 11.7 CI Workflow

`.github/workflows/smoke.yml` running `scripts/kind_smoke.sh` on push.
