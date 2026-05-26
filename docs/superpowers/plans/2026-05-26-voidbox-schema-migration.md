# void-box 0.2.x Schema Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate `void-shims` to consume the void-box 0.2.x `RunSpec` schema by pass-through, render Kubernetes Jobs whose KVM/mount knobs are auto-derived from `sandbox.{mode,mounts}`, and replace the manual minikube smoke flow with a committed `scripts/kind_smoke.sh`.

**Architecture:** `shim-core` becomes a thin re-export of `void_box::spec::*` (via `path` dependency on `../../../agent-infra/void-box`). `shim-k8s` keeps its CLI shape but `render_job_yaml` reads `sandbox.mode` and `sandbox.mounts` directly from the voidbox `RunSpec`. No isolation-profile enforcement (concept removed from void-box). agent-sandbox CRD backend deferred to a separate plan.

**Tech Stack:** Rust 2021 (workspace), clap 4, serde + serde_yaml, thiserror, uuid v4, base64. External: `void-box = "0.2.0"` (path dep), `kind`, `kubectl`, `docker`.

---

## File Structure

### Modified

- `crates/shim-core/Cargo.toml` — replace deps (drop serde/serde_yaml/uuid, add `void-box` path dep, keep thiserror).
- `crates/shim-core/src/lib.rs` — rewrite as re-export + new `ShimError` + `load_spec_for_shim` + `parse_spec_str`.
- `crates/shim-k8s/Cargo.toml` — no changes (already has serde_yaml, base64, clap, uuid, shim-core).
- `crates/shim-k8s/src/main.rs` — replace `--mount-kvm` with `--mount-kvm-override`; add `--mount-strategy`; rewrite `render_job_yaml` to read `sandbox.mode`/`sandbox.mounts`; new helper `derive_kvm_settings`, `render_volumes_for_mounts`, `normalize_dns1123_label`, `validate_mount_host`; new tests; delete tests obsoleted by schema change.
- `examples/run.yaml` — rewrite to `api_version: v1, kind: agent, sandbox.mode: mock`.
- `README.md` — replace "Minikube Smoke Test" section with "Local smoke (kind)".

### Created

- `scripts/kind_smoke.sh` — idempotent kind cluster + build + render + run + status + logs + rm, with cleanup trap.
- `spec/shim-v0.2.md` — content of v0.1 with §3 deleted, §4/§6/§7.1/§11 edited; the file `spec/shim-v0.1.md` is `git mv`'d into v0.2.md.

### Deleted (via rename)

- `spec/shim-v0.1.md` — replaced by `shim-v0.2.md`.

---

## Task 0: Verify prerequisites

**Files:** none (verification only).

- [ ] **Step 1: Verify void-box source tree is present at expected path**

Run:
```bash
test -f /home/diego/github/agent-infra/void-box/Cargo.toml && \
  grep '^version = "0.2.0"' /home/diego/github/agent-infra/void-box/Cargo.toml
```
Expected: line `version = "0.2.0"` printed; exit 0.

If this fails, STOP. The path dependency at `../../../agent-infra/void-box` (from `crates/shim-core/`) cannot resolve. Either clone void-box at that path or revise the plan.

- [ ] **Step 2: Verify the workspace currently builds**

Run from `/home/diego/github/void-shims`:
```bash
cargo build --workspace 2>&1 | tail -5
```
Expected: `Finished ... dev` (no errors). Baseline for comparison.

- [ ] **Step 3: Install kind and kubectl if missing**

Run:
```bash
mkdir -p ~/.local/bin
which kind   || curl -fsSL -o ~/.local/bin/kind    https://kind.sigs.k8s.io/dl/latest/kind-linux-amd64    && chmod +x ~/.local/bin/kind
which kubectl|| curl -fsSL -o ~/.local/bin/kubectl https://dl.k8s.io/release/$(curl -fsSL https://dl.k8s.io/release/stable.txt)/bin/linux/amd64/kubectl && chmod +x ~/.local/bin/kubectl
kind --version && kubectl version --client --output=yaml | head -2
```
Expected: both versions printed.

Note: `~/.local/bin` is already in PATH per environment context.

- [ ] **Step 4: Verify docker is running**

Run: `docker ps`
Expected: column header `CONTAINER ID` printed (table may be empty). If docker isn't running, start it.

---

## Task 1: shim-core — switch to void-box pass-through

**Files:**
- Modify: `crates/shim-core/Cargo.toml`
- Rewrite: `crates/shim-core/src/lib.rs`

- [ ] **Step 1: Write the failing test in new lib.rs**

Replace `crates/shim-core/src/lib.rs` entirely with this content:

```rust
//! Thin re-export of void-box's spec types, plus shim-specific error type.

pub use void_box::spec::{
    AgentSpec, LlmSpec, MountSpec, ObserveSpec, PipelineSpec, RunKind, RunSpec, SandboxSpec,
    WorkflowSpec,
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

/// Parse + validate a void-box RunSpec from a file path.
pub fn load_spec_for_shim(path: &std::path::Path) -> Result<RunSpec, ShimError> {
    Ok(void_box::spec::load_spec(path)?)
}

/// Parse a void-box RunSpec from a YAML string without running validation.
/// Use in tests; production callers must use `load_spec_for_shim`.
pub fn parse_spec_str(s: &str) -> Result<RunSpec, ShimError> {
    serde_yaml::from_str(s).map_err(|e| {
        ShimError::VoidBox(void_box::Error::Config(format!("yaml parse: {e}")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT_SPEC: &str = r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", timeout_secs: 5 }
"#;

    const WORKFLOW_SPEC: &str = r#"
api_version: v1
kind: workflow
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
workflow:
  steps:
    - name: s1
      run: { program: echo, args: ["hi"] }
"#;

    #[test]
    fn parses_kind_agent() {
        let spec = parse_spec_str(AGENT_SPEC).expect("agent spec parses");
        assert_eq!(spec.kind, RunKind::Agent);
        assert_eq!(spec.name, "t");
        assert!(spec.agent.is_some());
    }

    #[test]
    fn parses_kind_workflow() {
        let spec = parse_spec_str(WORKFLOW_SPEC).expect("workflow spec parses");
        assert_eq!(spec.kind, RunKind::Workflow);
        assert_eq!(spec.workflow.as_ref().unwrap().steps.len(), 1);
    }

    #[test]
    fn rejects_malformed_yaml() {
        let err = parse_spec_str("not: : valid: yaml::").expect_err("malformed yaml fails");
        assert!(matches!(err, ShimError::VoidBox(_)));
    }
}
```

- [ ] **Step 2: Update Cargo.toml**

Replace `crates/shim-core/Cargo.toml` entirely with:

```toml
[package]
name = "shim-core"
version = "0.2.0"
edition = "2021"
license = "Apache-2.0"

[dependencies]
void-box = { path = "../../../agent-infra/void-box" }
serde_yaml = "0.9"
thiserror = "1"
```

- [ ] **Step 3: Run tests — they should fail to compile because void-box re-export and `void_box::Error::Config` may not exist as written**

Run: `cargo test -p shim-core 2>&1 | tail -30`

Expected: build errors. Two likely failures to confirm/fix:
1. `void_box::Error::Config` — verify the actual variant in `/home/diego/github/agent-infra/void-box/src/error.rs`. If different (e.g. `Error::Spec` or has a different signature), update the `parse_spec_str` body and `parses_kind_*` tests accordingly.
2. The first build will be slow (void-box has ~132 transitive deps including kvm-ioctls). Expect 5–10 minutes on first compile.

- [ ] **Step 4: Read the actual void-box error type and fix `parse_spec_str` if needed**

Run: `grep -nE 'pub enum Error|pub struct Error' /home/diego/github/agent-infra/void-box/src/error.rs`

Inspect the file; pick the variant used for "bad config/spec" errors. Likely candidates: `Error::Config(String)` or `Error::Spec(String)`. If neither matches, fall back to wrapping via a fresh variant in `ShimError`:

```rust
#[error("yaml parse: {0}")]
Yaml(#[from] serde_yaml::Error),
```

and change `parse_spec_str` to:

```rust
pub fn parse_spec_str(s: &str) -> Result<RunSpec, ShimError> {
    Ok(serde_yaml::from_str(s)?)
}
```

- [ ] **Step 5: Run tests until they pass**

Run: `cargo test -p shim-core 2>&1 | tail -20`
Expected: `test result: ok. 3 passed; 0 failed`.

- [ ] **Step 6: Commit**

```bash
git add crates/shim-core/Cargo.toml crates/shim-core/src/lib.rs
git commit -m "shim-core: re-export void-box 0.2.x spec, drop legacy RunSpec"
```

---

## Task 2: shim-k8s — adapt render to new RunSpec, replace `--mount-kvm` flag

**Files:**
- Modify: `crates/shim-k8s/src/main.rs`

This task is large; broken into substeps. The whole task ends in one commit because the intermediate states don't compile.

- [ ] **Step 1: Replace CLI flags `--mount-kvm` with `--mount-kvm-override` and add `--mount-strategy`**

In `crates/shim-k8s/src/main.rs`, locate the `Render` and `Run` variants of `enum Command` (lines ~25 and ~50). For each:
- Remove the `mount_kvm: bool` field.
- Add:
  ```rust
  /// Force-override KVM auto-derivation. Default `auto` derives from sandbox.mode.
  #[arg(long, value_enum, default_value_t = MountKvmOverride::Auto)]
  mount_kvm_override: MountKvmOverride,
  /// How sandbox.mounts are materialized in the pod.
  #[arg(long, value_enum, default_value_t = MountStrategy::HostPath)]
  mount_strategy: MountStrategy,
  ```
- Change the default for `command` from `"void-box run --file /spec/run.yaml"` to `"voidbox run --file /spec/run.yaml"`.

Add these enums above `enum Command`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum MountKvmOverride { Auto, True, False }

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum MountStrategy { HostPath, EmptyDir }
```

- [ ] **Step 2: Update `RenderOptions` struct and the `match cli.cmd` arms that build it**

Replace the `RenderOptions` struct (around line ~162) with:

```rust
#[derive(Debug, Clone)]
struct RenderOptions {
    image: String,
    image_pull_policy: String,
    mount_kvm_override: MountKvmOverride,
    mount_strategy: MountStrategy,
    runtime_command: String,
}
```

In both the `Render` and `Run` arms of `match cli.cmd`, replace the `RenderOptions { ... }` construction with:

```rust
let opts = RenderOptions {
    image,
    image_pull_policy,
    mount_kvm_override,
    mount_strategy,
    runtime_command: command,
};
```

Replace the line `let run_id = spec.ensure_run_id();` (which calls a method that no longer exists) with:

```rust
let run_id = uuid::Uuid::new_v4();
```

- [ ] **Step 3: Rewrite `render_job_yaml` to read `sandbox.mode` and `sandbox.mounts`**

Replace the entire `render_job_yaml` function (lines ~442–518) with:

```rust
fn render_job_yaml(
    namespace: &str,
    job_name: &str,
    options: &RenderOptions,
    spec: &RunSpec,
    run_id: uuid::Uuid,
) -> Result<String, ShimError> {
    let run_yaml = serde_yaml::to_string(spec)
        .map_err(|e| ShimError::CommandFailed(format!("re-serialize spec: {e}")))?;
    let run_yaml_b64 = BASE64_STANDARD.encode(run_yaml.as_bytes());

    let mount_kvm = match options.mount_kvm_override {
        MountKvmOverride::Auto => derive_kvm_from_mode(&spec.sandbox.mode),
        MountKvmOverride::True => true,
        MountKvmOverride::False => false,
    };
    let privileged = mount_kvm;

    let (volumes_yaml, volume_mounts_yaml) =
        render_volumes_for_mounts(&spec.sandbox.mounts, options.mount_strategy, mount_kvm)?;

    let privileged_yaml = if privileged {
        "          securityContext:\n            privileged: true\n"
    } else {
        ""
    };

    let manifest = format!(
        r#"apiVersion: batch/v1
kind: Job
metadata:
  name: {job_name}
  namespace: {namespace}
  labels:
    app: void-shim-k8s
    void.run/id: "{run_id}"
spec:
  backoffLimit: 0
  template:
    metadata:
      labels:
        app: void-shim-k8s
        void.run/id: "{run_id}"
    spec:
      restartPolicy: Never
      containers:
        - name: void-box
          image: {image}
          imagePullPolicy: {image_pull_policy}
{privileged_yaml}          command: ["/bin/sh", "-lc"]
          args:
            - |
              set -eu
              mkdir -p /spec
              printf '%s' '{run_yaml_b64}' | base64 -d > /spec/run.yaml
              {runtime_command}
{volume_mounts_yaml}{volumes_yaml}"#,
        job_name = job_name,
        namespace = namespace,
        image = options.image,
        image_pull_policy = options.image_pull_policy,
        run_id = run_id,
        runtime_command = options.runtime_command,
        run_yaml_b64 = run_yaml_b64,
        privileged_yaml = privileged_yaml,
        volume_mounts_yaml = volume_mounts_yaml,
        volumes_yaml = volumes_yaml,
    );
    Ok(manifest)
}

fn derive_kvm_from_mode(mode: &str) -> bool {
    // void-box sandbox.mode values: auto | mock | kvm | vz
    matches!(mode, "auto" | "kvm")
}

fn render_volumes_for_mounts(
    mounts: &[shim_core::MountSpec],
    strategy: MountStrategy,
    include_kvm: bool,
) -> Result<(String, String), ShimError> {
    let mut volumes = String::new();
    let mut volume_mounts = String::new();

    let has_any = include_kvm || !mounts.is_empty();
    if !has_any {
        return Ok((String::new(), String::new()));
    }

    volume_mounts.push_str("          volumeMounts:\n");
    volumes.push_str("\n      volumes:\n");

    if include_kvm {
        volume_mounts.push_str("            - name: devkvm\n              mountPath: /dev/kvm\n");
        volumes.push_str("        - name: devkvm\n          hostPath:\n            path: /dev/kvm\n            type: CharDevice\n");
    }

    for (i, m) in mounts.iter().enumerate() {
        validate_mount_host(&m.host)?;
        let name = format!("mount-{i}");
        let read_only = m.mode == "ro";
        volume_mounts.push_str(&format!(
            "            - name: {name}\n              mountPath: {path}\n              readOnly: {ro}\n",
            name = name, path = m.host, ro = read_only
        ));
        match strategy {
            MountStrategy::HostPath => volumes.push_str(&format!(
                "        - name: {name}\n          hostPath:\n            path: {path}\n            type: DirectoryOrCreate\n",
                name = name, path = m.host
            )),
            MountStrategy::EmptyDir => volumes.push_str(&format!(
                "        - name: {name}\n          emptyDir: {{}}\n",
                name = name
            )),
        }
    }

    Ok((volumes, volume_mounts))
}

fn validate_mount_host(host: &str) -> Result<(), ShimError> {
    if !host.starts_with('/') {
        return Err(ShimError::InvalidMount(format!(
            "mount host path must be absolute, got: {host}"
        )));
    }
    Ok(())
}

fn normalize_dns1123_label(s: &str, max_len: usize) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    if out.len() > max_len {
        out.truncate(max_len);
    }
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "x".to_string();
    }
    out
}
```

- [ ] **Step 4: Update job name building to use normalization, and update the two `render_job_yaml` call sites to pass `run_id`**

Locate the two arms (`Command::Render { ... }` and `Command::Run { ... }`) in `fn main`. Replace the lines:

```rust
let run_id = spec.ensure_run_id();
let job_name = format!("{}-{}", name_prefix, short(run_id));
```

(In Step 2 you already replaced the first line.) Now replace the `job_name` line in BOTH arms with:

```rust
let name_part = normalize_dns1123_label(&spec.name, 30);
let job_name = format!("{}-{}-{}", normalize_dns1123_label(&name_prefix, 20), name_part, short(run_id));
```

And update the two `render_job_yaml(...)` calls to pass `run_id` as the fifth argument:

```rust
let yaml = render_job_yaml(&namespace, &job_name, &opts, &spec, run_id)?;
```

Also update both `Render` and `Run` arms to use `load_spec_for_shim` instead of `RunSpec::from_yaml_file`:

```rust
let spec = shim_core::load_spec_for_shim(&file)?;
```

(`let mut spec` is no longer needed — RunSpec no longer carries a mutable run_id.)

- [ ] **Step 5: Remove obsolete tests, add new tests**

In the `#[cfg(test)] mod tests` block at the bottom of `main.rs`:

- DELETE the `sample_spec()` helper (uses old schema).
- DELETE `render_includes_or_excludes_kvm_mount` (replaced by new tests below).
- KEEP `parse_run_ref_*`, `map_job_status_*`, `delete_args_*`, `logs_args_*`.
- ADD this content:

```rust
fn sample_agent_spec(mode: &str) -> shim_core::RunSpec {
    let yaml = format!(
        r#"
api_version: v1
kind: agent
name: smoke
sandbox: {{ mode: {mode}, memory_mb: 256, vcpus: 1 }}
agent: {{ prompt: "hi", timeout_secs: 5 }}
"#
    );
    shim_core::parse_spec_str(&yaml).expect("sample spec parses")
}

fn default_opts() -> RenderOptions {
    RenderOptions {
        image: "ghcr.io/the-void-ia/void-box:latest".to_string(),
        image_pull_policy: "IfNotPresent".to_string(),
        mount_kvm_override: MountKvmOverride::Auto,
        mount_strategy: MountStrategy::HostPath,
        runtime_command: "voidbox run --file /spec/run.yaml".to_string(),
    }
}

#[test]
fn render_includes_kvm_for_mode_kvm() {
    let spec = sample_agent_spec("kvm");
    let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
        .expect("render ok");
    assert!(yaml.contains("/dev/kvm"), "expected /dev/kvm mount; got:\n{yaml}");
    assert!(yaml.contains("privileged: true"), "expected privileged: true; got:\n{yaml}");
}

#[test]
fn render_excludes_kvm_for_mode_mock() {
    let spec = sample_agent_spec("mock");
    let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
        .expect("render ok");
    assert!(!yaml.contains("/dev/kvm"), "expected no /dev/kvm; got:\n{yaml}");
    assert!(!yaml.contains("privileged: true"), "expected no privileged; got:\n{yaml}");
}

#[test]
fn render_emits_volumes_for_sandbox_mounts() {
    let mut spec = sample_agent_spec("mock");
    spec.sandbox.mounts = vec![
        shim_core::MountSpec { host: "/tmp/a".into(), guest: "/a".into(), mode: "ro".into() },
        shim_core::MountSpec { host: "/tmp/b".into(), guest: "/b".into(), mode: "rw".into() },
    ];
    let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
        .expect("render ok");
    assert!(yaml.contains("name: mount-0"), "mount-0 missing in:\n{yaml}");
    assert!(yaml.contains("name: mount-1"), "mount-1 missing in:\n{yaml}");
    assert!(yaml.contains("path: /tmp/a"), "/tmp/a missing in:\n{yaml}");
    assert!(yaml.contains("readOnly: true"), "readOnly true missing in:\n{yaml}");
    assert!(yaml.contains("readOnly: false"), "readOnly false missing in:\n{yaml}");
}

#[test]
fn render_rejects_relative_host_path() {
    let mut spec = sample_agent_spec("mock");
    spec.sandbox.mounts = vec![shim_core::MountSpec {
        host: "rel/path".into(), guest: "/a".into(), mode: "ro".into(),
    }];
    let err = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
        .expect_err("relative path should fail");
    assert!(matches!(err, ShimError::InvalidMount(_)), "got {err:?}");
}

#[test]
fn render_includes_voidbox_command_default() {
    let spec = sample_agent_spec("mock");
    let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
        .expect("render ok");
    assert!(yaml.contains("voidbox run --file /spec/run.yaml"),
        "default command missing or wrong; got:\n{yaml}");
}

#[test]
fn normalize_dns1123_strips_invalid_chars() {
    assert_eq!(normalize_dns1123_label("My Run!", 30), "my-run");
    assert_eq!(normalize_dns1123_label("ABC_123", 30), "abc-123");
    assert_eq!(normalize_dns1123_label("---x---", 30), "x");
    assert_eq!(normalize_dns1123_label("", 30), "x");
}
```

- [ ] **Step 6: Run cargo build to surface any remaining errors**

Run: `cargo build -p shim-k8s 2>&1 | tail -30`

Fix any compile errors. Common issues:
- Unused import `serde::Deserialize` (already in the file; keep — it's used by `JobResource`).
- The `sample_spec()` helper still referenced — delete its usages or replace with `sample_agent_spec`.
- `spec.ensure_run_id()` referenced anywhere — there should be none after Step 2.

- [ ] **Step 7: Run tests**

Run: `cargo test -p shim-k8s 2>&1 | tail -30`
Expected: all tests pass.

- [ ] **Step 8: Manually verify render against a real spec**

Run:
```bash
cargo run -p shim-k8s -- render --file ./examples/run.yaml 2>&1 | head -30
```
Expected: The CURRENT `examples/run.yaml` is still the old v0.1 schema, so this should fail with a clear voidbox-spec error like "workflow: missing field `steps`". This is the expected handoff to Task 3.

- [ ] **Step 9: Commit**

```bash
git add crates/shim-k8s/src/main.rs
git commit -m "shim-k8s: render Job from void-box 0.2.x RunSpec, auto-derive KVM/mounts"
```

---

## Task 3: Replace `examples/run.yaml` with void-box 0.2.x agent spec

**Files:**
- Rewrite: `examples/run.yaml`

- [ ] **Step 1: Write the new example**

Replace `examples/run.yaml` entirely with:

```yaml
api_version: v1
kind: agent
name: smoke-test
sandbox:
  mode: mock
  memory_mb: 512
  vcpus: 1
llm:
  provider: claude
agent:
  prompt: "Say exactly: hello from shim-k8s. Then stop."
  timeout_secs: 60
```

- [ ] **Step 2: Verify it validates with both voidbox and the shim**

Run:
```bash
voidbox validate --file ./examples/run.yaml && \
  cargo run -p shim-k8s -- render --file ./examples/run.yaml > /tmp/job.yaml && \
  head -20 /tmp/job.yaml
```
Expected:
- `valid: ./examples/run.yaml (kind=Agent, api_version=v1)`
- A `Job` YAML rendered to /tmp/job.yaml with first 20 lines printed (no `/dev/kvm`, no `privileged`).

- [ ] **Step 3: Commit**

```bash
git add examples/run.yaml
git commit -m "examples: rewrite run.yaml to void-box 0.2.x agent spec (mode: mock)"
```

---

## Task 4: `scripts/kind_smoke.sh`

**Files:**
- Create: `scripts/kind_smoke.sh`

- [ ] **Step 1: Create the directory and script**

Create `scripts/kind_smoke.sh` with this exact content:

```bash
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
```

- [ ] **Step 2: Make it executable and check syntax**

Run:
```bash
mkdir -p scripts
chmod +x scripts/kind_smoke.sh
bash -n scripts/kind_smoke.sh && echo "syntax OK"
```
Expected: `syntax OK` printed.

- [ ] **Step 3: Dry-run the script up to the cluster step (without actually running)**

You can manually invoke up to the `kind create cluster` step to confirm prerequisite checks fire correctly. Skip if confident in `bash -n`.

- [ ] **Step 4: Commit**

```bash
git add scripts/kind_smoke.sh
git commit -m "scripts: add kind_smoke.sh for local end-to-end shim validation"
```

---

## Task 5: Rename and edit spec doc to v0.2

**Files:**
- Rename: `spec/shim-v0.1.md` → `spec/shim-v0.2.md`
- Edit: `spec/shim-v0.2.md`

- [ ] **Step 1: Rename**

Run:
```bash
git mv spec/shim-v0.1.md spec/shim-v0.2.md
```

- [ ] **Step 2: Edit version header**

In `spec/shim-v0.2.md`, change line 3 from `Version: v0.1 Status: Draft` to `Version: v0.2 Status: Draft`.

- [ ] **Step 3: Replace §3 entirely**

Find the section starting with `## 3. Core Execution Profile (Mandatory)` and replace the entire §3 (down to but not including `## 4.`) with:

```markdown
## 3. Runtime Model

void-box 0.2.x runs one microVM per `voidbox run` invocation by construction.
Shims do not enforce an isolation profile — the RunSpec is consumed verbatim
and executed as-is by the void-box binary inside the container/pod/domain
managed by the shim.

------------------------------------------------------------------------
```

- [ ] **Step 4: Replace §4.1**

Find `### 4.1 Input: RunSpec` and replace its body (until `### 4.2`) with:

```markdown
### 4.1 Input: RunSpec

Shims accept the void-box `RunSpec` schema verbatim (`api_version: v1`,
`kind: agent|workflow|pipeline|sandbox`, see the void-box repository for
the canonical definition). No shim-specific transformations beyond what is
documented in §7 (target requirements).

```

- [ ] **Step 5: Update §6**

Find `## 6. Runtime Invocation Contract`. Replace the example block (the `void-box run --file workflow.yaml --execution-profile shim-workflow-per-vm` line) with:

```markdown
    voidbox run --file /spec/run.yaml

```

- [ ] **Step 6: Update §7.1**

Find `### 7.1 void-shim-k8s (MVP Target)`. Append after the existing "Status mapping" block, just before the `------` separator:

```markdown
Auto-derivation:
- `securityContext.privileged` and `/dev/kvm` mount are auto-derived from
  `sandbox.mode` (`auto`/`kvm` → enabled, `mock`/`vz` → disabled). Override
  with `--mount-kvm-override`.
- `sandbox.mounts[]` are rendered as paired `volumes` + `volumeMounts`. The
  default strategy is `hostPath`; override with `--mount-strategy emptyDir`
  for ephemeral pod-local storage (e.g. CI smoke tests).

```

- [ ] **Step 7: Append to §11**

Find `## 11. Next Steps (Post-MVP)`. Append a new subsection at the end:

```markdown
### 11.6 agent-sandbox CRD Backend

Add a second renderer for the kubernetes-sigs `agents.x-k8s.io/v1beta1`
`Sandbox` CRD, for specs with `kind: agent, mode: service|interactive` or
`kind: sandbox` where long-running pods with suspend/resume semantics are
the right model. Selector flag: `--backend job|sandbox|auto`. Tracked in a
separate design doc.

### 11.7 CI Workflow

`.github/workflows/smoke.yml` running `scripts/kind_smoke.sh` on push.
```

- [ ] **Step 8: Commit**

```bash
git add spec/shim-v0.1.md spec/shim-v0.2.md
git commit -m "spec: bump to v0.2, drop isolation_mode mandate, document auto-derivation"
```

(`git add` of the deleted file is required because `git mv` records both halves.)

---

## Task 6: Update README

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Replace the "Minikube Smoke Test" section**

In `README.md`, find the section starting with `## Minikube Smoke Test` (line 41 in the current file). Replace the entire section (including its code block and ending before `## Notes`) with:

```markdown
## Local Smoke (kind)

End-to-end validation against a local `kind` cluster. The default example
spec uses `sandbox.mode: mock`, so no `/dev/kvm` access is required.

Prereqs: `docker` running, `kind` and `kubectl` on PATH.

```bash
./scripts/kind_smoke.sh           # cleans up cluster on exit
./scripts/kind_smoke.sh --keep-cluster  # leaves cluster running for debugging
```

The script:
1. Ensures a `void-shims-smoke` kind cluster exists
2. Builds `shim-k8s` (release)
3. Renders `examples/run.yaml` and dry-run applies the manifest
4. If the container image is reachable, runs the Job, polls status,
   fetches logs, and deletes the Job

Set `SHIM_IMAGE=<other-image>` to override the container image.

```

- [ ] **Step 2: Update the "Run the Kubernetes shim (render mode)" snippet**

Find the existing `cargo run -p shim-k8s -- render ...` block (around line 22). It still works as-is; no edit needed unless you also want to delete the trailing `> job.yaml` to avoid littering the repo root. Optional: change to `> /tmp/job.yaml`.

- [ ] **Step 3: Update the "Next Steps" section**

Find `## Next Steps` (around line 68) and replace its bullet list with:

```markdown
- Add `--image-pull-secret` support in `shim-k8s` for private GHCR images.
- Add runtime mode for images without `/bin/sh` (direct entrypoint/args support).
- Add a second backend renderer for the `agents.x-k8s.io/v1beta1 Sandbox` CRD.
- GitHub Actions workflow that runs `scripts/kind_smoke.sh` on push.
- Factor `void-box::spec` into a sub-crate so `shim-core` can switch from
  `path` to `git` dependency.
```

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: README — kind smoke replaces minikube; next steps refreshed"
```

---

## Task 7: Final verification

**Files:** none (verification only).

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace 2>&1 | tail -5`
Expected: `Finished ... dev`.

- [ ] **Step 2: Full workspace tests**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: all tests pass across `shim-core`, `shim-k8s`, `shim-containerd`, `shim-libvirt`.

- [ ] **Step 3: Voidbox roundtrip**

Run:
```bash
voidbox validate --file ./examples/run.yaml && \
  cargo run -p shim-k8s -- render --file ./examples/run.yaml > /tmp/void-shims-rendered.yaml && \
  kubectl apply --dry-run=client -f /tmp/void-shims-rendered.yaml 2>&1 || \
    echo "(kubectl dry-run only works if kubectl is installed; non-fatal here)"
```
Expected: `valid: ...` line and `job.batch/void-run-smoke-test-<uuid> created (dry run)` (or the kubectl-not-installed fallback). No "missing field" errors.

- [ ] **Step 4: Optional — full kind smoke**

Run: `./scripts/kind_smoke.sh`
Expected: `smoke OK (dry-run only)` (most likely, since the image probably isn't published with voidbox 0.2.x yet) or `smoke OK (live run Succeeded)`.

- [ ] **Step 5: Inspect commit history**

Run: `git log --oneline`
Expected: ~7 commits on `feature/voidbox-0.2-migration`, all atomic, all with subject-line ≤ 70 chars, no Co-Authored-By trailers.

---

## Self-review notes

- Spec coverage:
  - "Pass-through with path dependency" → Task 1.
  - "Drop isolation_mode enforcement" → Task 1 (lib.rs no longer carries it) + Task 5 §3 rewrite.
  - "Auto-derive privileged + /dev/kvm" → Task 2 (`derive_kvm_from_mode`).
  - "sandbox.mounts → k8s volumes + volumeMounts" → Task 2 (`render_volumes_for_mounts`).
  - "Job-only backend in this patch" → no Sandbox CRD task; mentioned in §11.6 of v0.2 spec and README Next Steps.
  - "kind smoke" → Task 4.
  - New example with `mode: mock` → Task 3.
  - Spec doc v0.2 edits → Task 5.
  - README → Task 6.
- Placeholder scan: clean (no TBD/TODO/"appropriate handling" — every code block is concrete).
- Type consistency: `MountKvmOverride`, `MountStrategy`, `RenderOptions` field names, `render_job_yaml` signature (5 args incl. `run_id`), `shim_core::MountSpec` field names (`host`/`guest`/`mode`) all line up across Tasks 1, 2, 4, 5.
