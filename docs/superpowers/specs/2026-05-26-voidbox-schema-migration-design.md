# void-shims migration to void-box 0.2.x spec

Date: 2026-05-26
Status: Draft (pending review)
Branch: `feature/voidbox-0.2-migration`

## Problem

`void-shims` was written against an older "shim v0.1" `RunSpec` schema
(`run_id`, `workflow.file`, `execution.isolation_mode`, top-level
`mounts/env`). The installed `void-box` (source: `/home/diego/github/agent-infra/void-box`,
version `0.2.0`) uses an incompatible schema:
`api_version: v1`, `kind: agent|workflow|pipeline|sandbox`, `name`,
`sandbox: { mode, mounts, env, ... }`, plus kind-specific sections
(`agent`, `workflow`, `pipeline`).

In addition, the binary the shim invokes inside the container was renamed
`void-box` → `voidbox`, and the `workflow_per_vm` isolation profile no longer
exists as a concept (every `voidbox run` is one VM per RunSpec by construction).

As a result, the example `examples/run.yaml` shipped with `void-shims` fails
`voidbox validate` and the rendered Kubernetes Job invokes a non-existent
binary inside the container.

## Goals

1. Make `void-shims` consume the void-box 0.2.x `RunSpec` schema verbatim
   (pass-through), so users author specs once and they work both with the
   `voidbox` CLI directly and with `void-shim-k8s`.
2. Auto-derive Kubernetes runtime knobs (privileged/`/dev/kvm`, volume mounts)
   from the spec's `sandbox` section rather than from independent CLI flags.
3. Replace the manual "minikube smoke" instructions with a committed,
   idempotent `scripts/kind_smoke.sh` exercising
   `render → run → status → logs → rm` against a local `kind` cluster.
4. Update `spec/shim-v0.1.md` to `v0.2` removing references to the now-defunct
   `workflow_per_vm` isolation profile.

## Non-goals

- Changes to `shim-containerd` or `shim-libvirt`. They remain stubs.
- GitHub Actions CI pipeline for the smoke test (left as follow-up).
- KVM-on-CI / nested virtualization. The kind smoke uses `sandbox.mode: mock`.
- `agent-sandbox` CRD backend (`agents.x-k8s.io/v1beta1` Sandbox). Tracked
  as future work — see [Future work](#future-work).
- Removing the `path` dependency on `void-box`. Documented as a known
  tradeoff with a follow-up path; see [Risks and follow-ups](#risks-and-follow-ups).

## Decisions and rationale

### Pass-through with path dependency

`shim-core` re-exports `void_box::spec::*` rather than defining its own
mirrored types. This guarantees zero schema drift: whatever `voidbox`
accepts, the shim accepts. Cost: `void-box = { path = "../../../agent-infra/void-box" }`
makes the repo non-buildable on machines that don't have void-box cloned at the
same relative path. Accepted as a "for now" tradeoff, with the clean future
fix being an upstream change to void-box (factor `spec` into a sub-crate
with no heavy transitive deps), then switching to `{ git = "...", tag = "v0.2.0" }`.

### Drop `isolation_mode` enforcement

void-box 0.2.x does not model isolation profiles. Every `voidbox run` is
one VM per RunSpec. The shim's previous "enforce `workflow_per_vm`" logic
is therefore a no-op against the new binary. We delete the code path and
update `spec/shim-v0.1.md` (renamed to `shim-v0.2.md`) to remove the
mandate.

### Auto-derive `privileged` + `/dev/kvm` from `sandbox.mode`

| `sandbox.mode` | `securityContext.privileged` | mount `/dev/kvm` |
|---|---|---|
| `mock` | false | no |
| `auto` | true | yes |
| `kvm` | true | yes |
| `vz` | false | no |
| (absent) | treated as `auto` | yes |

Override flag: `--mount-kvm-override auto|true|false` (default `auto`).
Removes the standalone `--mount-kvm` boolean flag.

### `sandbox.mounts` → k8s `volumes` + `volumeMounts`

Each `MountSpec { host, guest, mode }` becomes a paired `hostPath` volume
and `volumeMount`. The volume name is auto-generated as `mount-N` to
guarantee DNS-1123 validity, regardless of any user-chosen identifier.
The container's `volumeMount.mountPath` is set to `host` (not `guest`),
because `voidbox` runs *inside* the pod and is what translates host→guest
into the inner microVM.

| Spec field | k8s rendering |
|---|---|
| `mounts[i].host` | `volumes[i].hostPath.path` + `volumeMounts[i].mountPath` |
| `mounts[i].guest` | not rendered (consumed by voidbox inside the pod) |
| `mounts[i].mode == "ro"` | `volumeMounts[i].readOnly: true` |
| `mounts[i].mode == "rw"` (default differs: voidbox default is `ro`) | `volumeMounts[i].readOnly: false` |

Validation: `host` must be absolute, else `ShimError::InvalidMount(...)`.

### Job-only backend in this patch; Sandbox CRD as follow-up

The shim continues to render `batch/v1 Job`. A second backend targeting the
`agents.x-k8s.io/v1beta1 Sandbox` CRD (long-running, suspendable pods, ideal
for `kind: agent, mode: service` and `kind: sandbox`) is in scope for a
separate design and patch. This decision keeps the migration patch focused.

## Architecture

```
void-shims (workspace)
├── crates/shim-core
│   ├── Cargo.toml          # adds: void-box = { path = "../../../agent-infra/void-box" }
│   └── src/lib.rs          # ~50 LOC: re-export void_box::spec::*, ShimError, load_spec_for_shim
├── crates/shim-k8s
│   ├── Cargo.toml          # unchanged deps
│   └── src/main.rs         # render_job_yaml reads sandbox.mode/mounts; new tests
├── crates/shim-containerd  # unchanged
├── crates/shim-libvirt     # unchanged
├── examples/run.yaml       # rewritten to api_version:v1 kind:agent (mode: mock)
├── scripts/kind_smoke.sh   # new
├── spec/shim-v0.2.md       # renamed from shim-v0.1.md, edits below
└── README.md               # smoke section rewritten for kind
```

### `shim-core` surface

```rust
// crates/shim-core/src/lib.rs
pub use void_box::spec::{
    RunSpec, RunKind, SandboxSpec, MountSpec,
    AgentSpec, WorkflowSpec, PipelineSpec, LlmSpec, ObserveSpec,
    // ... (full re-export of spec module)
};

#[derive(thiserror::Error, Debug)]
pub enum ShimError {
    #[error("voidbox spec error: {0}")]
    VoidBox(#[from] void_box::Error),
    #[error("invalid run ref: {0}")]
    InvalidRunRef(String),
    #[error("invalid mount: {0}")]
    InvalidMount(String),
    #[error("required command not found: {0}")]
    CommandNotFound(String),
    #[error("command failed: {0}")]
    CommandFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

pub fn load_spec_for_shim(path: &std::path::Path) -> Result<RunSpec, ShimError> {
    Ok(void_box::spec::load_spec(path)?)
}
```

Deleted: `RunSpec`, `WorkflowRef`, `Execution`, `Mount`, `default_isolation_mode`,
`ensure_run_id`, `from_yaml_str`, `from_yaml_file`, and their tests.

### `shim-k8s` surface

CLI unchanged in shape:

```
void-shim-k8s render --file <spec> [--namespace] [--name-prefix] [--image]
                                    [--image-pull-policy] [--mount-kvm-override auto|true|false]
                                    [--command] [--mount-strategy hostpath|emptydir]
void-shim-k8s run    --file <spec> [same flags]
void-shim-k8s status <run_ref>
void-shim-k8s logs   <run_ref> [--follow]
void-shim-k8s rm     <run_ref> [--force]
```

Changes:
- Default `--command` becomes `voidbox run --file /spec/run.yaml`.
- Removed `--mount-kvm` (replaced by `--mount-kvm-override`).
- New `--mount-strategy hostpath|emptydir` (default `hostpath`); affects how
  `sandbox.mounts` are materialized.
- Job name unchanged in shape: `{name_prefix}-{8-char-uuid}`. UUID is generated
  fresh on every `run`/`render` invocation since void-box 0.2.x specs do not
  carry a `run_id`.

`render_job_yaml` now takes `&RunSpec` (from voidbox), reads `spec.sandbox.mode`
and `spec.sandbox.mounts`, and emits volumes/volumeMounts accordingly.

## Example spec (new `examples/run.yaml`)

```yaml
api_version: v1
kind: agent
name: smoke-test
sandbox:
  mode: mock        # no /dev/kvm required, works in any cluster
  memory_mb: 512
  vcpus: 1
llm:
  provider: claude
agent:
  prompt: "Say exactly: hello from shim-k8s. Then stop."
  timeout_secs: 60
```

Validates with both `voidbox validate --file examples/run.yaml` and
`void-shim-k8s render --file examples/run.yaml`.

## kind smoke (`scripts/kind_smoke.sh`)

Idempotent, cleans up on EXIT trap (unless `--keep-cluster`).

Pipeline:
1. Ensure `kind` cluster `void-shims-smoke` exists, wait 60s for ready.
2. `cargo build -p shim-k8s --release`.
3. `target/release/void-shim-k8s render --file examples/run.yaml | kubectl apply --dry-run=client -f -` (manifest sanity).
4. Detect container image: `docker manifest inspect ghcr.io/the-void-ia/void-box:latest`; if absent, log and skip `run` (script exits 0 after dry-run validation).
5. If image present: `run`, poll `status` until terminal, `logs`, `rm`.

Prereqs (documented in README):
- `docker` running
- `kind` binary in PATH (`~/.local/bin/kind`)
- `kubectl` binary in PATH (`~/.local/bin/kubectl`)

If a prereq is missing the script prints a one-line install hint and exits 2.

## Error handling

| Situation | Variant | Notes |
|---|---|---|
| Spec invalid per voidbox | `VoidBox(void_box::Error)` | Preserves file:line from `void_box::spec::load_spec`. |
| `sandbox.mounts[i].host` not absolute | `InvalidMount(...)` | Loud failure; no normalization. |
| `name` not DNS-1123 | (silent normalization) | Lowercased, invalid chars → `-`, truncated to 63. Documented in CLI help. |
| `kubectl` not on PATH | `CommandNotFound("kubectl")` | Exit nonzero. |
| `kubectl apply/get/delete` non-zero | `CommandFailed(...)` with stderr | No retry, no inferred semantics. |
| Job/Pod not found on `status`/`logs`/`rm` | `CommandFailed` | We surface raw kubectl error; do not invent "not found" handling. |

No silent fallback for declared mounts (mandate from `spec/shim-v0.2.md` §8 inherited from v0.1).

## Tests

### Unit (in-crate)

`shim-core`:
- `load_spec_for_shim` happy path for `kind: agent`.
- `load_spec_for_shim` happy path for `kind: workflow`.
- `load_spec_for_shim` with malformed YAML → `Err(VoidBox(_))`.
- `load_spec_for_shim` missing required `name` → `Err(VoidBox(_))` with message containing "name".

`shim-k8s` (in addition to existing `parse_run_ref`, `map_job_to_state`, `build_logs_args`, `build_delete_job_args` which continue to apply):
- `render_includes_kvm_for_mode_kvm`: spec with `sandbox.mode: kvm` → manifest contains `/dev/kvm` and `privileged: true`.
- `render_excludes_kvm_for_mode_mock`: spec with `sandbox.mode: mock` → manifest does not contain `/dev/kvm` and does not set `privileged: true`.
- `render_emits_volumes_for_sandbox_mounts`: spec with two mounts → manifest emits two `volumes` and two `volumeMounts` with names `mount-0`, `mount-1`.
- `render_rejects_relative_host_path`: mount with `host: ./rel` → `Err(InvalidMount(_))`.
- `render_normalizes_invalid_name`: spec with `name: "My Run!"` → job name becomes `void-run-my-run-<uuid>`.
- `render_includes_voidbox_command_default`: rendered container args contain `voidbox run --file /spec/run.yaml`.
- `render_includes_or_excludes_kvm_mount` (existing) ported to new spec shape.

Deleted tests (no longer apply): `enforces_workflow_per_vm`, `rejects_empty_workflow_file`, `generates_run_id_when_missing`.

### Integration smoke

`scripts/kind_smoke.sh` as above. Manual invocation only in this patch.

## Spec doc edits (`spec/shim-v0.1.md` → `spec/shim-v0.2.md`)

- Version header: `v0.2`.
- §3 "Core Execution Profile" — delete entirely. Replace with one sentence:
  "void-box 0.2.x runs one microVM per `voidbox run` invocation by
  construction; shims do not need to enforce an isolation profile."
- §4.1 "Input: RunSpec" — replace contents with: "Shims accept the void-box
  `RunSpec` schema verbatim (`api_version: v1`, `kind: agent|workflow|pipeline|sandbox`,
  see void-box repo for canonical definition). No shim-specific transformations
  beyond what is documented in this spec's §7 (target requirements)."
- §6 "Runtime Invocation Contract" — update example to
  `voidbox run --file /spec/run.yaml` (remove `--execution-profile`).
- §7.1 "void-shim-k8s" — add: "auto-derives `privileged` and `/dev/kvm`
  mount from `sandbox.mode`, generates k8s volume per `sandbox.mounts[i]`
  (default strategy: `hostPath`)."
- §11 "Next Steps" — append new items: registry secrets, agent-sandbox CRD
  backend, CI workflow.

## Future work

1. **`agent-sandbox` CRD backend** — render `agents.x-k8s.io/v1beta1 Sandbox`
   instead of `batch/v1 Job` for specs with `kind: agent, mode: service|interactive`
   or `kind: sandbox`. Adds `--backend job|sandbox|auto`, `suspend`/`resume`
   subcommands, status mapping to Sandbox conditions. Requires the
   agent-sandbox controller installed on cluster. Separate design doc.
2. **`void-box-spec` sub-crate** — upstream change to void-box that factors
   the `spec` module into a tiny sibling crate with no kvm/vmm deps. Once
   shipped, switch `shim-core`'s dependency from `path` to
   `{ git = "...", tag = "vX.Y" }` and the cross-machine build issue is gone.
3. **GitHub Actions CI** — `.github/workflows/smoke.yml` running `kind_smoke.sh`
   on push. Already drafted in section 5 of brainstorming, deferred for velocity.
4. **`shim-containerd` and `shim-libvirt`** — still stubs; scope to be defined
   when use cases materialize.
5. **Registry pull secrets** — for private GHCR voidbox images; surface via
   `--image-pull-secret` on `render`/`run`.

## Risks and follow-ups

- **Path dependency on `void-box` is fragile.** Anyone cloning `void-shims`
  also needs `void-box` at `../../agent-infra/void-box` to build. Mitigation:
  follow-up #2 above.
- **Container image availability.** The default
  `ghcr.io/the-void-ia/void-box:latest` may not yet contain voidbox 0.2.x
  with kernel/initramfs baked in. Smoke script auto-detects and degrades
  gracefully (dry-run only). Real end-to-end run depends on the image being
  published; tracked separately.
- **`sandbox.mode: kvm` on kind/k3d.** Nested KVM is not available by default
  in kind/k3d. The smoke uses `mode: mock` to sidestep this. Real KVM testing
  requires a bare-metal node or a specially configured VM host.
- **Schema drift across void-box minor versions.** Pass-through means any
  field added to void-box `RunSpec` becomes available in `void-shims` for
  free, but a renamed or removed field breaks the shim at the path-dep level.
  Mitigation: pin to a specific void-box tag once we switch from `path` to
  `git` dependency.
