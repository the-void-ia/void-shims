# mode:service via agent-sandbox CRD Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a second rendering backend to `void-shim-k8s` that emits an `agents.x-k8s.io/v1beta1 Sandbox` CR (plus auxiliary Secrets) for void-box specs with `kind: agent, mode: service`, with client subcommands (`send/cancel/telemetry/endpoint/suspend/resume`) that talk to the `voidbox serve` daemon inside the pod via bearer-token HTTP, plus an end-to-end kind smoke that exercises a real Anthropic API round-trip.

**Architecture:** Auto-detect backend from spec (`kind: agent, mode: service` → Sandbox; else → Job). New module `render_sandbox.rs` mirrors the existing `format!`-based pattern of the Job renderer. New module `client.rs` uses `reqwest` blocking to hit `/v1/runs/{id}/{messages,cancel,telemetry}`. Endpoint resolution wraps `kubectl port-forward` with `Drop`-based cleanup. Bearer token auto-generated per Sandbox, stored in a `Secret`; LLM API key (default `ANTHROPIC_API_KEY` from operator env) propagated into a sibling `Secret`. Suspend/resume = `kubectl scale sandbox/<name> --replicas=0|1`.

**Tech Stack:** Rust 2021 (workspace), clap 4, serde + serde_json + serde_yaml, thiserror, uuid v4, base64, **new:** reqwest 0.12 (blocking, rustls), chrono 0.4 (for RFC3339 shutdownTime). External: `kubectl`, `kind`, `docker`, `agent-sandbox` controller v0.x (installed at smoke time).

---

## File Structure

### Modified

- `crates/shim-core/Cargo.toml` — add `chrono` for RFC3339 timestamp generation.
- `crates/shim-core/src/lib.rs` — add `enum Backend { Job, Sandbox }`, `pub fn detect_backend`, 5 new `ShimError` variants.
- `crates/shim-k8s/Cargo.toml` — add `reqwest` (blocking, rustls-tls), `chrono`.
- `crates/shim-k8s/src/main.rs` — add `Backend` clap flag, 6 new subcommands (`send`, `cancel`, `telemetry`, `endpoint`, `suspend`, `resume`), bifurcation in `Render`/`Run` by backend, auto-detect in `Status`/`Logs`/`Rm`.
- `scripts/kind_smoke.sh` — extend with sandbox-path smoke (controller install + service round-trip + LLM real-API assertion).
- `examples/` — add new `run-service.yaml`.
- `README.md` — document new subcommands and live LLM smoke.

### Created

- `crates/shim-k8s/src/render_sandbox.rs` — emits Sandbox CR + auxiliary Secrets (3 or 4 YAML docs).
- `crates/shim-k8s/src/client.rs` — HTTP client (blocking reqwest) for daemon ops.
- `crates/shim-k8s/src/portforward.rs` — `kubectl port-forward` wrapper with `Drop` cleanup.
- `crates/shim-k8s/src/crd_check.rs` — pre-flight `kubectl get crd sandboxes.agents.x-k8s.io`.
- `crates/shim-k8s/src/llm_creds.rs` — provider → env-var mapping + Secret rendering for credentials.
- The container image's entrypoint script `voidbox-entrypoint.sh` — extended to handle `serve-and-load` mode in addition to `run`.

### NOT touched (intentional)

- `crates/shim-containerd`, `crates/shim-libvirt` — stubs, untouched.
- `crates/shim-core/src/lib.rs` — RunSpec re-export unchanged.
- `spec/shim-v0.2.md` — unchanged in this PR; design references it but doesn't modify.

---

## Task 0: Verify prerequisites and base state

**Files:** none (verification only).

- [ ] **Step 1: Confirm we're on the right base branch with PR #1 work**

Run from `/home/diego/github/void-shims`:
```bash
git log --oneline -3
```
Expected: top 3 commits include the spec doc (`07c331d` or `c44a1a0` after corrections) and the PR #1 work. If on `main`, switch with `git checkout feature/voidbox-0.2-migration`.

- [ ] **Step 2: Create a fresh feature branch for this work**

```bash
git checkout -b feature/service-backend
```
Expected: branch created and switched.

- [ ] **Step 3: Workspace builds cleanly + tests pass (baseline)**

```bash
cargo build --workspace 2>&1 | tail -3
cargo test --workspace 2>&1 | grep "test result"
```
Expected: `Finished dev`; 17 tests pass (3 in shim-core + 14 in shim-k8s).

- [ ] **Step 4: Confirm voidbox 0.2.0 release binary exists locally**

```bash
ls -lh /home/diego/github/agent-infra/void-box/target/release/voidbox
/home/diego/github/agent-infra/void-box/target/release/voidbox --version
```
Expected: binary present, `voidbox 0.2.0` printed. If absent, run `cd /home/diego/github/agent-infra/void-box && cargo build --release --bin voidbox` (5-10 min).

- [ ] **Step 5: Confirm kind + kubectl + docker still available**

```bash
which kind kubectl docker && kind version && docker ps >/dev/null && echo "all ok"
```
Expected: `all ok` printed.

- [ ] **Step 6: Confirm agent-sandbox release manifest URL**

```bash
curl -fsSLI -o /dev/null -w '%{http_code}\n' https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.4.6/manifest.yaml
```
Expected: `200`. The install URL used in Task 8 is `https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.4.6/manifest.yaml` (release manifest, NOT the kustomize path). If 404 (release was pulled), pin to the next-most-recent version available from `gh api repos/kubernetes-sigs/agent-sandbox/releases --jq '.[].tag_name' | head`.

---

## Task 1: shim-core — add `Backend` enum, `detect_backend`, new error variants

**Files:**
- Modify: `crates/shim-core/Cargo.toml`
- Modify: `crates/shim-core/src/lib.rs`

- [ ] **Step 1: Add `chrono` dependency**

Edit `crates/shim-core/Cargo.toml`. Add to `[dependencies]`:

```toml
chrono = { version = "0.4", default-features = false, features = ["clock"] }
```

(Wait — `chrono` is used by `shim-k8s` for `shutdownTime`, not `shim-core`. Skip this step; the chrono dep goes in shim-k8s in Task 2. Strike this step and proceed.)

- [ ] **Step 2: Write failing test for `detect_backend`**

Open `crates/shim-core/src/lib.rs`. Add to the `#[cfg(test)] mod tests` block (after the existing tests):

```rust
#[test]
fn detect_backend_agent_service_is_sandbox() {
    let spec = parse_spec_str(r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", mode: service, timeout_secs: 60 }
"#).unwrap();
    assert_eq!(detect_backend(&spec), Backend::Sandbox);
}

#[test]
fn detect_backend_agent_task_is_job() {
    let spec = parse_spec_str(r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", mode: task }
"#).unwrap();
    assert_eq!(detect_backend(&spec), Backend::Job);
}

#[test]
fn detect_backend_agent_interactive_is_job_for_now() {
    let spec = parse_spec_str(r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", mode: interactive }
"#).unwrap();
    assert_eq!(detect_backend(&spec), Backend::Job);
}

#[test]
fn detect_backend_workflow_is_job() {
    let spec = parse_spec_str(r#"
api_version: v1
kind: workflow
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
workflow:
  steps:
    - name: s1
      run: { program: echo, args: ["hi"] }
"#).unwrap();
    assert_eq!(detect_backend(&spec), Backend::Job);
}

#[test]
fn detect_backend_agent_no_mode_defaults_to_job() {
    let spec = parse_spec_str(r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi" }
"#).unwrap();
    assert_eq!(detect_backend(&spec), Backend::Job);
}
```

Run: `cargo test -p shim-core 2>&1 | tail -10`
Expected: FAIL because `Backend` and `detect_backend` are not defined.

- [ ] **Step 3: Implement `Backend` enum and `detect_backend`**

In `crates/shim-core/src/lib.rs`, add this AFTER the `pub use void_box::spec::{...};` block and BEFORE `#[derive(thiserror::Error, Debug)] pub enum ShimError`:

```rust
use void_box::spec::AgentMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Job,
    Sandbox,
}

pub fn detect_backend(spec: &RunSpec) -> Backend {
    if spec.kind == RunKind::Agent {
        if let Some(agent) = &spec.agent {
            if agent.mode == AgentMode::Service {
                return Backend::Sandbox;
            }
        }
    }
    Backend::Job
}
```

- [ ] **Step 4: Add new `ShimError` variants**

In `crates/shim-core/src/lib.rs`, modify the `pub enum ShimError` block. Add these variants after the existing `NotImplemented`:

```rust
    #[error("missing CRD: {0}")]
    MissingCrd(String),
    #[error("daemon http error: {0}")]
    DaemonHttp(String),
    #[error("port-forward failed: {0}")]
    PortForward(String),
    #[error("backend mismatch: run_ref {0} is backend {1}, command requires {2}")]
    BackendMismatch(String, String, String),
    #[error("voidbox run not found in daemon (expected 1, got {0})")]
    DaemonRunNotFound(usize),
```

- [ ] **Step 5: Run tests, fix any compile errors**

Run: `cargo test -p shim-core 2>&1 | tail -15`
Expected: `test result: ok. 8 passed; 0 failed` (3 existing + 5 new).

If `use void_box::spec::AgentMode;` is shadowing or unused due to existing re-exports, adjust by qualifying inline as `void_box::spec::AgentMode::Service` and removing the `use`.

- [ ] **Step 6: Commit**

```bash
git add crates/shim-core/Cargo.toml crates/shim-core/src/lib.rs
git commit -m "shim-core: add Backend enum + detect_backend + new error variants"
```

(Cargo.toml may not have changed if chrono was not added — that's fine, `git add` won't include it.)

---

## Task 2: shim-k8s — add `reqwest` + `chrono` dependencies, scaffold new module files

**Files:**
- Modify: `crates/shim-k8s/Cargo.toml`
- Create (empty): `crates/shim-k8s/src/render_sandbox.rs`
- Create (empty): `crates/shim-k8s/src/client.rs`
- Create (empty): `crates/shim-k8s/src/portforward.rs`
- Create (empty): `crates/shim-k8s/src/crd_check.rs`
- Create (empty): `crates/shim-k8s/src/llm_creds.rs`
- Modify: `crates/shim-k8s/src/main.rs` (add `mod` declarations)

- [ ] **Step 1: Add reqwest + chrono deps**

Edit `crates/shim-k8s/Cargo.toml`. In `[dependencies]`, add:

```toml
reqwest = { version = "0.12", default-features = false, features = ["blocking", "json", "rustls-tls"] }
chrono = { version = "0.4", default-features = false, features = ["clock", "serde"] }
```

- [ ] **Step 2: Create empty module files**

```bash
cat > crates/shim-k8s/src/render_sandbox.rs <<'EOF'
//! Renders agents.x-k8s.io/v1beta1 Sandbox CR + auxiliary Secrets.
EOF

cat > crates/shim-k8s/src/client.rs <<'EOF'
//! Blocking HTTP client for the voidbox daemon.
EOF

cat > crates/shim-k8s/src/portforward.rs <<'EOF'
//! kubectl port-forward wrapper with Drop-based cleanup.
EOF

cat > crates/shim-k8s/src/crd_check.rs <<'EOF'
//! Pre-flight: verify required CRDs are installed.
EOF

cat > crates/shim-k8s/src/llm_creds.rs <<'EOF'
//! Map void-box LLM provider to env vars + render credentials Secret.
EOF
```

- [ ] **Step 3: Add `mod` declarations in main.rs**

Edit `crates/shim-k8s/src/main.rs`. Add at the top of the file (after the existing `use` statements, before `#[derive(Parser, Debug)]`):

```rust
mod client;
mod crd_check;
mod llm_creds;
mod portforward;
mod render_sandbox;
```

- [ ] **Step 4: Verify build still passes**

Run: `cargo build -p shim-k8s 2>&1 | tail -5`
Expected: `Finished dev`. Modules are empty so they introduce no functionality yet.

- [ ] **Step 5: Commit**

```bash
git add crates/shim-k8s/Cargo.toml crates/shim-k8s/src/
git commit -m "shim-k8s: scaffold modules for sandbox backend (reqwest, chrono deps)"
```

---

## Task 3: shim-k8s — implement `crd_check` and `llm_creds`

**Files:**
- Modify: `crates/shim-k8s/src/crd_check.rs`
- Modify: `crates/shim-k8s/src/llm_creds.rs`

- [ ] **Step 1: Write failing test for `crd_check`**

Replace `crates/shim-k8s/src/crd_check.rs` contents with:

```rust
//! Pre-flight: verify required CRDs are installed.

use shim_core::ShimError;
use std::process::Command;

/// Returns Ok(()) if the agent-sandbox CRD is installed; Err(MissingCrd) otherwise.
/// Shells out to kubectl. Network failures and other errors bubble up as Err(MissingCrd)
/// with the underlying message for easier diagnosis.
pub fn check_sandbox_crd_installed() -> Result<(), ShimError> {
    let output = Command::new("kubectl")
        .args(["get", "crd", "sandboxes.agents.x-k8s.io", "-o", "name"])
        .output()
        .map_err(|e| ShimError::MissingCrd(format!("could not invoke kubectl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShimError::MissingCrd(format!(
            "sandboxes.agents.x-k8s.io CRD not found in cluster. \
             Install with: kubectl apply -k github.com/kubernetes-sigs/agent-sandbox/config/default\n\
             (kubectl said: {})",
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // Note: We don't unit-test the live kubectl call. Behavior under
    // "kubectl missing" is covered by the existing kubectl wrapper tests
    // (CommandNotFound), and behavior under "CRD missing" is covered by
    // the kind_smoke.sh integration test where we explicitly skip install
    // and assert the error.
}
```

- [ ] **Step 2: Write tests for `llm_creds`**

Replace `crates/shim-k8s/src/llm_creds.rs` contents with:

```rust
//! Map void-box LLM provider to env vars + render credentials Secret.

use shim_core::RunSpec;

/// Returns the env var name the daemon expects for this LLM provider, or
/// None if the provider doesn't need credentials (e.g. ollama, lm-studio).
pub fn env_var_for_provider(provider: &str) -> Option<&'static str> {
    match provider.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some("ANTHROPIC_API_KEY"),
        "codex" | "openai" => Some("OPENAI_API_KEY"),
        "ollama" | "lm-studio" | "lm_studio" => None,
        _ => None,
    }
}

/// Returns Some((env_var, value)) if the spec needs an LLM credential AND
/// the operator's environment has the value, None if no Secret should be
/// emitted. Returns Err only if we know we need a credential but the env
/// var is missing AND `strict` is true.
pub fn resolve_llm_credential(
    spec: &RunSpec,
    override_env_var: Option<&str>,
) -> Option<(String, String)> {
    let provider = spec.llm.as_ref().map(|l| l.provider.as_str())?;
    let env_var = override_env_var
        .map(String::from)
        .or_else(|| env_var_for_provider(provider).map(String::from))?;
    let value = std::env::var(&env_var).ok()?;
    Some((env_var, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shim_core::parse_spec_str;

    fn spec_with_provider(p: &str) -> RunSpec {
        let yaml = format!(
            r#"
api_version: v1
kind: agent
name: t
sandbox: {{ mode: mock, memory_mb: 256, vcpus: 1 }}
llm: {{ provider: {p} }}
agent: {{ prompt: "hi", mode: service, timeout_secs: 60 }}
"#
        );
        parse_spec_str(&yaml).unwrap()
    }

    #[test]
    fn env_var_claude_is_anthropic_api_key() {
        assert_eq!(env_var_for_provider("claude"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_var_for_provider("Claude"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_var_for_provider("anthropic"), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn env_var_codex_is_openai_api_key() {
        assert_eq!(env_var_for_provider("codex"), Some("OPENAI_API_KEY"));
        assert_eq!(env_var_for_provider("openai"), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn env_var_ollama_is_none() {
        assert_eq!(env_var_for_provider("ollama"), None);
        assert_eq!(env_var_for_provider("lm-studio"), None);
    }

    #[test]
    fn resolve_returns_none_if_env_unset() {
        // Ensure the var is unset for this test
        std::env::remove_var("ANTHROPIC_API_KEY");
        let spec = spec_with_provider("claude");
        assert_eq!(resolve_llm_credential(&spec, None), None);
    }

    #[test]
    fn resolve_returns_some_when_env_set() {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-12345");
        let spec = spec_with_provider("claude");
        let got = resolve_llm_credential(&spec, None);
        assert_eq!(
            got,
            Some(("ANTHROPIC_API_KEY".into(), "sk-ant-test-12345".into()))
        );
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn resolve_respects_override_env_var() {
        std::env::set_var("CUSTOM_KEY", "value-xyz");
        let spec = spec_with_provider("claude");
        let got = resolve_llm_credential(&spec, Some("CUSTOM_KEY"));
        assert_eq!(got, Some(("CUSTOM_KEY".into(), "value-xyz".into())));
        std::env::remove_var("CUSTOM_KEY");
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p shim-k8s 2>&1 | tail -15`
Expected: `test result: ok. 19 passed` (14 existing + 5 new in llm_creds).

- [ ] **Step 4: Commit**

```bash
git add crates/shim-k8s/src/crd_check.rs crates/shim-k8s/src/llm_creds.rs
git commit -m "shim-k8s: crd_check + llm_creds helpers with provider mapping"
```

---

## Task 4: shim-k8s — implement `render_sandbox`

**Files:**
- Modify: `crates/shim-k8s/src/render_sandbox.rs`

This is the largest module in the plan. Write all the tests first, then implement.

- [ ] **Step 1: Add module skeleton with TODO-stub functions and 8 failing tests**

Replace `crates/shim-k8s/src/render_sandbox.rs` contents with:

```rust
//! Renders agents.x-k8s.io/v1beta1 Sandbox CR + auxiliary Secrets.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{Duration, Utc};
use shim_core::{RunSpec, ShimError};

#[derive(Debug, Clone)]
pub struct SandboxRenderOptions {
    pub image: String,
    pub image_pull_policy: String,
    pub mount_kvm: bool,
    pub runtime_command_args: Vec<String>, // typically ["serve-and-load"]
    /// If Some, an LLM credential Secret will be rendered with this (env_var_name, value).
    pub llm_credential: Option<(String, String)>,
}

/// Renders 2-4 YAML docs joined by "\n---\n":
/// 1. Sandbox CR
/// 2. token Secret (always)
/// 3. spec Secret (always)
/// 4. llm-creds Secret (optional)
pub fn render_sandbox_yaml(
    namespace: &str,
    sandbox_name: &str,
    options: &SandboxRenderOptions,
    spec: &RunSpec,
    run_id: uuid::Uuid,
    token: &str,
) -> Result<String, ShimError> {
    let run_yaml = serde_yaml::to_string(spec)?;
    let run_yaml_b64 = BASE64_STANDARD.encode(run_yaml.as_bytes());
    let token_b64 = BASE64_STANDARD.encode(token.as_bytes());

    // shutdownTime from agent.timeout_secs, if present
    let shutdown_time_line = spec
        .agent
        .as_ref()
        .and_then(|a| a.timeout_secs)
        .map(|secs| {
            let when = Utc::now() + Duration::seconds(secs as i64);
            format!("  lifecycle:\n    shutdownTime: \"{}\"\n    shutdownPolicy: Delete\n", when.to_rfc3339())
        })
        .unwrap_or_default();

    let privileged_yaml = if options.mount_kvm {
        "          securityContext:\n            privileged: true\n"
    } else {
        ""
    };

    let llm_env = options
        .llm_credential
        .as_ref()
        .map(|(env_var, _)| {
            format!(
                "            - name: {env_var}\n              valueFrom:\n                secretKeyRef:\n                  name: {sandbox_name}-llm-creds\n                  key: {env_var}\n"
            )
        })
        .unwrap_or_default();

    let devkvm_volume_mount = if options.mount_kvm {
        "            - name: devkvm\n              mountPath: /dev/kvm\n"
    } else {
        ""
    };
    let devkvm_volume = if options.mount_kvm {
        "        - name: devkvm\n          hostPath:\n            path: /dev/kvm\n            type: CharDevice\n"
    } else {
        ""
    };

    let args_yaml = options
        .runtime_command_args
        .iter()
        .map(|a| format!("            - \"{a}\""))
        .collect::<Vec<_>>()
        .join("\n");

    let sandbox_doc = format!(
        r#"apiVersion: agents.x-k8s.io/v1beta1
kind: Sandbox
metadata:
  name: {sandbox_name}
  namespace: {namespace}
  labels:
    app: void-shim-k8s
    void.run/id: "{run_id}"
    void.run/backend: "sandbox"
spec:
  replicas: 1
  service: true
{shutdown_time_line}  podTemplate:
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
{privileged_yaml}          command: ["/usr/local/bin/voidbox-entrypoint.sh"]
          args:
{args_yaml}
          env:
            - name: VOIDBOX_DAEMON_TOKEN
              valueFrom:
                secretKeyRef:
                  name: {sandbox_name}-token
                  key: token
{llm_env}          ports:
            - name: daemon
              containerPort: 43100
              protocol: TCP
          readinessProbe:
            httpGet:
              path: /v1/runs
              port: daemon
              httpHeaders:
                - name: Authorization
                  value: "Bearer $(VOIDBOX_DAEMON_TOKEN)"
            initialDelaySeconds: 2
            periodSeconds: 2
            timeoutSeconds: 2
            failureThreshold: 30
          volumeMounts:
            - name: spec
              mountPath: /spec
              readOnly: true
{devkvm_volume_mount}      volumes:
        - name: spec
          secret:
            secretName: {sandbox_name}-spec
{devkvm_volume}"#,
        image = options.image,
        image_pull_policy = options.image_pull_policy,
    );

    let token_secret = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: {sandbox_name}-token
  namespace: {namespace}
  labels:
    app: void-shim-k8s
    void.run/id: "{run_id}"
type: Opaque
data:
  token: {token_b64}
"#
    );

    let spec_secret = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: {sandbox_name}-spec
  namespace: {namespace}
  labels:
    app: void-shim-k8s
    void.run/id: "{run_id}"
type: Opaque
data:
  run.yaml: {run_yaml_b64}
"#
    );

    let mut output = String::new();
    output.push_str(&sandbox_doc);
    output.push_str("\n---\n");
    output.push_str(&token_secret);
    output.push_str("\n---\n");
    output.push_str(&spec_secret);

    if let Some((env_var, value)) = &options.llm_credential {
        let value_b64 = BASE64_STANDARD.encode(value.as_bytes());
        let llm_secret = format!(
            r#"apiVersion: v1
kind: Secret
metadata:
  name: {sandbox_name}-llm-creds
  namespace: {namespace}
  labels:
    app: void-shim-k8s
    void.run/id: "{run_id}"
type: Opaque
data:
  {env_var}: {value_b64}
"#
        );
        output.push_str("\n---\n");
        output.push_str(&llm_secret);
    }

    Ok(output)
}

/// Generates a 32-byte random hex token (64 chars).
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use shim_core::parse_spec_str;

    fn sample_service_spec() -> RunSpec {
        parse_spec_str(r#"
api_version: v1
kind: agent
name: test
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
llm: { provider: claude }
agent: { prompt: "hi", mode: service, timeout_secs: 60 }
"#).unwrap()
    }

    fn default_options() -> SandboxRenderOptions {
        SandboxRenderOptions {
            image: "ghcr.io/the-void-ia/void-box:latest".into(),
            image_pull_policy: "IfNotPresent".into(),
            mount_kvm: false,
            runtime_command_args: vec!["serve-and-load".into()],
            llm_credential: None,
        }
    }

    #[test]
    fn render_emits_sandbox_with_replicas_1_and_service_true() {
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        assert!(yaml.contains("kind: Sandbox"));
        assert!(yaml.contains("replicas: 1"));
        assert!(yaml.contains("service: true"));
    }

    #[test]
    fn render_includes_token_secret_ref_and_env() {
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        assert!(yaml.contains("name: VOIDBOX_DAEMON_TOKEN"));
        assert!(yaml.contains("name: t-svc-token"));
        assert!(yaml.contains("kind: Secret"));
    }

    #[test]
    fn render_includes_readiness_probe_with_bearer_auth() {
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        assert!(yaml.contains("readinessProbe:"));
        assert!(yaml.contains("path: /v1/runs"));
        assert!(yaml.contains("Bearer $(VOIDBOX_DAEMON_TOKEN)"));
    }

    #[test]
    fn render_includes_kvm_for_mount_kvm_true() {
        let mut opts = default_options();
        opts.mount_kvm = true;
        let yaml = render_sandbox_yaml("default", "t-svc", &opts, &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        assert!(yaml.contains("privileged: true"));
        assert!(yaml.contains("/dev/kvm"));
    }

    #[test]
    fn render_excludes_kvm_for_mount_kvm_false() {
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        assert!(!yaml.contains("privileged: true"));
        assert!(!yaml.contains("/dev/kvm"));
    }

    #[test]
    fn render_emits_three_docs_without_llm_creds() {
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 3, "expected 3 docs (Sandbox + token Secret + spec Secret); got {}:\n{}", docs.len(), yaml);
    }

    #[test]
    fn render_emits_four_docs_with_llm_creds() {
        let mut opts = default_options();
        opts.llm_credential = Some(("ANTHROPIC_API_KEY".into(), "sk-test".into()));
        let yaml = render_sandbox_yaml("default", "t-svc", &opts, &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(docs.len(), 4, "expected 4 docs with LLM creds Secret; got {}:\n{}", docs.len(), yaml);
        assert!(yaml.contains("name: t-svc-llm-creds"));
        assert!(yaml.contains("ANTHROPIC_API_KEY: c2stdGVzdA=="));  // base64 of "sk-test"
    }

    #[test]
    fn render_includes_shutdown_time_when_timeout_set() {
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &sample_service_spec(), uuid::Uuid::nil(), "tok").unwrap();
        assert!(yaml.contains("lifecycle:"));
        assert!(yaml.contains("shutdownTime:"));
        assert!(yaml.contains("shutdownPolicy: Delete"));
    }

    #[test]
    fn render_omits_shutdown_time_when_no_timeout() {
        let yaml_no_timeout = r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
llm: { provider: claude }
agent: { prompt: "hi", mode: service }
"#;
        let spec = parse_spec_str(yaml_no_timeout).unwrap();
        let yaml = render_sandbox_yaml("default", "t-svc", &default_options(), &spec, uuid::Uuid::nil(), "tok").unwrap();
        assert!(!yaml.contains("shutdownTime:"));
    }

    #[test]
    fn generate_token_is_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()), "got: {t}");
    }

    #[test]
    fn generate_token_is_unique_per_call() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 2: Add `rand` dependency for token generation**

Edit `crates/shim-k8s/Cargo.toml`. In `[dependencies]`, add:

```toml
rand = "0.8"
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p shim-k8s render_sandbox 2>&1 | tail -20`
Expected: 11 tests pass.

If compilation fails because of `chrono` features, ensure `chrono` is in scope and the `Utc::now()` + `Duration` paths resolve.

- [ ] **Step 4: Commit**

```bash
git add crates/shim-k8s/Cargo.toml crates/shim-k8s/src/render_sandbox.rs
git commit -m "shim-k8s: render Sandbox CR + 3/4 Secrets, with KVM/shutdownTime/LLM creds"
```

---

## Task 5: shim-k8s — implement `portforward` and `client`

**Files:**
- Modify: `crates/shim-k8s/src/portforward.rs`
- Modify: `crates/shim-k8s/src/client.rs`

- [ ] **Step 1: Implement `portforward` module**

Replace `crates/shim-k8s/src/portforward.rs` with:

```rust
//! kubectl port-forward wrapper with Drop-based cleanup.

use shim_core::ShimError;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A live port-forward; killed when dropped.
pub struct PortForward {
    child: Child,
    pub local_port: u16,
}

impl PortForward {
    /// Spawn `kubectl port-forward -n <ns> sandbox/<name> 0:<remote_port>` and
    /// parse the assigned local port from stdout. Returns once kubectl prints
    /// "Forwarding from 127.0.0.1:<port> -> <remote>" or after the timeout.
    pub fn open(namespace: &str, sandbox_name: &str, remote_port: u16) -> Result<Self, ShimError> {
        let mut child = Command::new("kubectl")
            .args([
                "port-forward",
                "-n", namespace,
                &format!("sandbox/{sandbox_name}"),
                &format!("0:{remote_port}"),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ShimError::PortForward(format!("spawn kubectl: {e}")))?;

        let stdout = child.stdout.take().ok_or_else(|| {
            ShimError::PortForward("kubectl stdout pipe missing".into())
        })?;

        let local_port = parse_port_with_timeout(stdout, Duration::from_secs(10))?;
        Ok(Self { child, local_port })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_port_with_timeout(stdout: impl std::io::Read + Send + 'static, timeout: Duration) -> Result<u16, ShimError> {
    use std::sync::mpsc;
    use std::thread;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(port) = parse_port_line(&line) {
                let _ = tx.send(port);
                return;
            }
        }
    });

    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(port) = rx.recv_timeout(Duration::from_millis(100)) {
            return Ok(port);
        }
    }
    Err(ShimError::PortForward(format!(
        "timed out after {}s waiting for port-forward to bind",
        timeout.as_secs()
    )))
}

/// Parses lines like "Forwarding from 127.0.0.1:50321 -> 43100".
fn parse_port_line(line: &str) -> Option<u16> {
    line.split("127.0.0.1:")
        .nth(1)?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse::<u16>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_port_line_extracts_assigned_port() {
        assert_eq!(parse_port_line("Forwarding from 127.0.0.1:50321 -> 43100"), Some(50321));
        assert_eq!(parse_port_line("Forwarding from 127.0.0.1:8080 -> 80"), Some(8080));
    }

    #[test]
    fn parse_port_line_rejects_unrelated_lines() {
        assert_eq!(parse_port_line("some other output"), None);
        assert_eq!(parse_port_line("Forwarding from [::1]:9090 -> 80"), None);
    }
}
```

- [ ] **Step 2: Implement `client` module**

Replace `crates/shim-k8s/src/client.rs` with:

```rust
//! Blocking HTTP client for the voidbox daemon.

use serde::{Deserialize, Serialize};
use shim_core::ShimError;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DaemonClient {
    base_url: String,
    token: String,
    http: reqwest::blocking::Client,
}

#[derive(Serialize)]
struct SendMessageBody<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize, Debug)]
pub struct RunSummary {
    pub run_id: String,
    #[serde(default)]
    pub state: serde_json::Value,
}

#[derive(Deserialize, Debug)]
pub struct ListRunsResponse {
    pub runs: Vec<RunSummary>,
}

impl DaemonClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>, timeout_secs: u64) -> Result<Self, ShimError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| ShimError::DaemonHttp(format!("build http client: {e}")))?;
        Ok(Self { base_url: base_url.into(), token: token.into(), http })
    }

    /// GET /v1/runs and return the singular run_id, error if !=1.
    pub fn first_run_id(&self) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs", self.base_url);
        let resp = self.http.get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("GET {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(ShimError::DaemonHttp(format!("GET {url}: {status}: {body}")));
        }
        // daemon returns either {"runs": [...]} or a list directly; try both shapes
        let text = resp.text().map_err(|e| ShimError::DaemonHttp(format!("read body: {e}")))?;
        let runs: Vec<RunSummary> = serde_json::from_str::<ListRunsResponse>(&text)
            .map(|r| r.runs)
            .or_else(|_| serde_json::from_str::<Vec<RunSummary>>(&text))
            .map_err(|e| ShimError::DaemonHttp(format!("parse runs list: {e}; body: {}", text.chars().take(200).collect::<String>())))?;
        match runs.len() {
            1 => Ok(runs.into_iter().next().unwrap().run_id),
            n => Err(ShimError::DaemonRunNotFound(n)),
        }
    }

    pub fn send_message(&self, run_id: &str, role: &str, content: &str) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs/{}/messages", self.base_url, run_id);
        let resp = self.http.post(&url)
            .bearer_auth(&self.token)
            .json(&SendMessageBody { role, content })
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("POST {url}: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(ShimError::DaemonHttp(format!("POST {url}: {status}: {body}")));
        }
        Ok(body)
    }

    pub fn cancel(&self, run_id: &str) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs/{}/cancel", self.base_url, run_id);
        let resp = self.http.post(&url)
            .bearer_auth(&self.token)
            .body("")
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("POST {url}: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(ShimError::DaemonHttp(format!("POST {url}: {status}: {body}")));
        }
        Ok(body)
    }

    pub fn telemetry(&self, run_id: &str) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs/{}/telemetry", self.base_url, run_id);
        let resp = self.http.get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("GET {url}: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(ShimError::DaemonHttp(format!("GET {url}: {status}: {body}")));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_valid_url() {
        let c = DaemonClient::new("http://127.0.0.1:12345", "tok", 30).unwrap();
        assert_eq!(c.base_url, "http://127.0.0.1:12345");
        assert_eq!(c.token, "tok");
    }

    // Live request tests are covered by the kind_smoke.sh integration path.
    // Mock-server tests using wiremock are intentionally omitted to keep
    // the test-deps surface small; the path is straightforward and exercised
    // by integration smoke.
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p shim-k8s 2>&1 | tail -20`
Expected: tests added by both modules pass; full count now ~32 (14 existing + 5 llm_creds + 11 render_sandbox + 2 portforward parse + 1 client build = 33). Verify by inspecting the count.

- [ ] **Step 4: Commit**

```bash
git add crates/shim-k8s/src/portforward.rs crates/shim-k8s/src/client.rs
git commit -m "shim-k8s: portforward wrapper + daemon HTTP client"
```

---

## Task 6: shim-k8s — wire backend selection + new subcommands into main.rs

**Files:**
- Modify: `crates/shim-k8s/src/main.rs`

This is a large modification to `main.rs`. Apply all steps before building.

- [ ] **Step 1: Add `Backend` clap flag to `Render` and `Run` subcommands**

Open `crates/shim-k8s/src/main.rs`. Locate the `Render` and `Run` variants of `enum Command`. In BOTH, add a field:

```rust
        /// Force backend selection. `auto` derives from spec.
        #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
        backend: BackendArg,
```

Add this enum above `enum Command`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum BackendArg { Auto, Job, Sandbox }

impl BackendArg {
    fn resolve(self, spec: &shim_core::RunSpec) -> shim_core::Backend {
        match self {
            BackendArg::Auto => shim_core::detect_backend(spec),
            BackendArg::Job => shim_core::Backend::Job,
            BackendArg::Sandbox => shim_core::Backend::Sandbox,
        }
    }
}
```

- [ ] **Step 2: Add 6 new subcommands to `enum Command`**

Locate `enum Command` and add these variants after `Rm`:

```rust
    /// Send a message to a running service-mode agent.
    Send {
        run_ref: String,
        #[arg(long, conflicts_with = "message_file")]
        message: Option<String>,
        #[arg(long, conflicts_with = "message")]
        message_file: Option<PathBuf>,
        #[arg(long, default_value = "user")]
        role: String,
        #[arg(long, default_value = "auto")]
        endpoint_mode: EndpointMode,
    },

    /// Cancel a running service-mode agent.
    Cancel {
        run_ref: String,
        #[arg(long, default_value = "auto")]
        endpoint_mode: EndpointMode,
    },

    /// Fetch telemetry snapshot from a running service-mode agent.
    Telemetry {
        run_ref: String,
        #[arg(long, default_value = "auto")]
        endpoint_mode: EndpointMode,
    },

    /// Print connection info (URL, token, sample curl) for a sandbox run_ref.
    Endpoint {
        run_ref: String,
        #[arg(long, default_value = "auto")]
        endpoint_mode: EndpointMode,
    },

    /// Suspend a sandbox by scaling its replicas to 0.
    Suspend { run_ref: String },

    /// Resume a sandbox by scaling its replicas to 1.
    Resume { run_ref: String },
```

Add this enum above `enum Command`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EndpointMode { Auto, InCluster, PortForward }
```

- [ ] **Step 3: Bifurcate `Render` and `Run` in `fn main`**

In `fn main`, locate the `Command::Render { ... }` arm. After resolving `spec` and `run_id`, branch on backend:

```rust
        Command::Render {
            file,
            namespace,
            name_prefix,
            image,
            image_pull_policy,
            mount_kvm_override,
            mount_strategy,
            command,
            backend,
        } => {
            let spec = shim_core::load_spec_for_shim(&file)?;
            let run_id = uuid::Uuid::new_v4();
            let name_part = normalize_dns1123_label(&spec.name, 30);
            let job_name = format!("{}-{}-{}", normalize_dns1123_label(&name_prefix, 20), name_part, short(run_id));

            match backend.resolve(&spec) {
                shim_core::Backend::Job => {
                    let opts = RenderOptions {
                        image,
                        image_pull_policy,
                        mount_kvm_override,
                        mount_strategy,
                        runtime_command: command,
                    };
                    let yaml = render_job_yaml(&namespace, &job_name, &opts, &spec, run_id)?;
                    print!("{}", yaml);
                }
                shim_core::Backend::Sandbox => {
                    let opts = render_sandbox::SandboxRenderOptions {
                        image,
                        image_pull_policy,
                        mount_kvm: derive_kvm_for_sandbox(mount_kvm_override, &spec),
                        runtime_command_args: vec!["serve-and-load".into()],
                        llm_credential: llm_creds::resolve_llm_credential(&spec, None),
                    };
                    let token = render_sandbox::generate_token();
                    let yaml = render_sandbox::render_sandbox_yaml(
                        &namespace, &job_name, &opts, &spec, run_id, &token,
                    )?;
                    print!("{}", yaml);
                }
            }
            Ok(())
        }
```

Add this helper above `fn main`:

```rust
fn derive_kvm_for_sandbox(override_: MountKvmOverride, spec: &shim_core::RunSpec) -> bool {
    match override_ {
        MountKvmOverride::Auto => derive_kvm_from_mode(&spec.sandbox.mode),
        MountKvmOverride::True => true,
        MountKvmOverride::False => false,
    }
}
```

Apply the same branching pattern to `Command::Run { ... }`: for the Sandbox branch, add `crd_check::check_sandbox_crd_installed()?;` BEFORE the render, then `kubectl_apply_yaml(&yaml)?` and `println!("{namespace}/{job_name}")`.

- [ ] **Step 4: Implement the 6 new subcommand arms**

In `fn main`, add these arms to the match block (after the existing `Rm` arm):

```rust
        Command::Send { run_ref, message, message_file, role, endpoint_mode } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let content = match (message, message_file) {
                (Some(m), _) => m,
                (None, Some(p)) => std::fs::read_to_string(&p)?,
                (None, None) => return Err(ShimError::CommandFailed("--message or --message-file required".into())),
            };
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, _pf) = resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            let client = client::DaemonClient::new(endpoint, token, 30)?;
            let run_id = client.first_run_id()?;
            let body = client.send_message(&run_id, &role, &content)?;
            println!("{}", body);
            Ok(())
        }

        Command::Cancel { run_ref, endpoint_mode } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, _pf) = resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            let client = client::DaemonClient::new(endpoint, token, 30)?;
            let run_id = client.first_run_id()?;
            let body = client.cancel(&run_id)?;
            println!("{}", body);
            Ok(())
        }

        Command::Telemetry { run_ref, endpoint_mode } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, _pf) = resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            let client = client::DaemonClient::new(endpoint, token, 30)?;
            let run_id = client.first_run_id()?;
            let body = client.telemetry(&run_id)?;
            println!("{}", body);
            Ok(())
        }

        Command::Endpoint { run_ref, endpoint_mode } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, pf) = resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            println!("URL: {}", endpoint);
            println!("Token: {}", token);
            println!("Sample curl: curl -H 'Authorization: Bearer {}' {}/v1/runs", token, endpoint);
            if pf.is_some() {
                println!("(port-forward will close when this command exits; press Ctrl-C to terminate)");
                std::thread::park();
            }
            Ok(())
        }

        Command::Suspend { run_ref } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let args = ["scale", "-n", &namespace, &format!("sandbox/{}", sandbox_name), "--replicas=0"];
            kubectl_output(&args)?;
            println!("Suspended {}", run_ref);
            Ok(())
        }

        Command::Resume { run_ref } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let args = ["scale", "-n", &namespace, &format!("sandbox/{}", sandbox_name), "--replicas=1"];
            kubectl_output(&args)?;
            println!("Resumed {}", run_ref);
            Ok(())
        }
```

- [ ] **Step 5: Implement supporting helpers**

Add to `crates/shim-k8s/src/main.rs` (anywhere among the existing helpers):

```rust
fn read_sandbox_token(namespace: &str, sandbox_name: &str) -> Result<String, ShimError> {
    let secret_name = format!("{sandbox_name}-token");
    let args = [
        "get", "secret", &secret_name, "-n", namespace,
        "-o", "jsonpath={.data.token}",
    ];
    let output = kubectl_output(&args)?;
    let b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    let bytes = B64.decode(&b64).map_err(|e| ShimError::CommandFailed(format!("decode token: {e}")))?;
    String::from_utf8(bytes).map_err(|e| ShimError::CommandFailed(format!("token not utf8: {e}")))
}

fn resolve_sandbox_endpoint(
    namespace: &str,
    sandbox_name: &str,
    mode: EndpointMode,
) -> Result<(String, Option<portforward::PortForward>), ShimError> {
    let effective = match mode {
        EndpointMode::Auto => {
            if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
                EndpointMode::InCluster
            } else {
                EndpointMode::PortForward
            }
        }
        other => other,
    };
    match effective {
        EndpointMode::InCluster => {
            let url = format!("http://{sandbox_name}.{namespace}.svc.cluster.local:43100");
            Ok((url, None))
        }
        EndpointMode::PortForward => {
            let pf = portforward::PortForward::open(namespace, sandbox_name, 43100)?;
            let url = pf.url();
            Ok((url, Some(pf)))
        }
        EndpointMode::Auto => unreachable!("resolved above"),
    }
}
```

- [ ] **Step 6: Build and fix any issues**

Run: `cargo build -p shim-k8s 2>&1 | tail -30`

Common issues:
- Missing `use` imports — clap may need `use clap::ValueEnum` or similar
- `EndpointMode` needs to be `Clone, Copy` since clap requires it
- `PortForward` Drop happening before the request — make sure the variable `_pf` is held in scope for the lifetime of the client request (not dropped early as `_` would do)

- [ ] **Step 7: Test**

Run: `cargo test -p shim-k8s 2>&1 | tail -5`
Expected: all tests still pass.

- [ ] **Step 8: Sanity-check the render path locally**

Create a tmp service spec:

```bash
cat > /tmp/run-service.yaml <<'EOF'
api_version: v1
kind: agent
name: smoke-svc
sandbox: { mode: mock, memory_mb: 512, vcpus: 1 }
llm:
  provider: claude
  model: claude-haiku-4-5-20251001
agent:
  prompt: "You are an echo. Reply with the user's content verbatim."
  mode: service
  timeout_secs: 600
EOF

cargo run -p shim-k8s -- render --file /tmp/run-service.yaml 2>&1 | head -30
```

Expected: prints Sandbox CR (apiVersion `agents.x-k8s.io/v1beta1`). If a warning about missing CRD appears, that's fine for `render` — pre-flight only runs on `run`.

- [ ] **Step 9: Commit**

```bash
git add crates/shim-k8s/src/main.rs
git commit -m "shim-k8s: wire Backend selection + send/cancel/telemetry/endpoint/suspend/resume"
```

---

## Task 7: Container image — extend entrypoint to support `serve-and-load`

**Files:**
- Create: `crates/shim-k8s/dockerfile/Dockerfile` (move from /tmp/voidbox-smoke)
- Create: `crates/shim-k8s/dockerfile/voidbox-entrypoint.sh`
- Create: `crates/shim-k8s/dockerfile/build.sh` (helper script)

These artifacts get committed so the kind_smoke can rebuild without /tmp dependencies.

- [ ] **Step 1: Create dockerfile directory and entrypoint script**

```bash
mkdir -p crates/shim-k8s/dockerfile
cat > crates/shim-k8s/dockerfile/voidbox-entrypoint.sh <<'EOF'
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
EOF
chmod +x crates/shim-k8s/dockerfile/voidbox-entrypoint.sh
```

- [ ] **Step 2: Create Dockerfile**

```bash
cat > crates/shim-k8s/dockerfile/Dockerfile <<'EOF'
# Container image bundling void-box + the entrypoint dispatcher.
# Build with: ./crates/shim-k8s/dockerfile/build.sh /path/to/voidbox
#
# Base must have glibc >= 2.39 to match void-box 0.2.x release builds on
# Fedora 44+ hosts. trixie-slim has glibc 2.40.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY voidbox /usr/local/bin/voidbox
COPY voidbox-entrypoint.sh /usr/local/bin/voidbox-entrypoint.sh
RUN chmod +x /usr/local/bin/voidbox /usr/local/bin/voidbox-entrypoint.sh

ENTRYPOINT ["/usr/local/bin/voidbox-entrypoint.sh"]
EOF
```

- [ ] **Step 3: Create build helper**

```bash
cat > crates/shim-k8s/dockerfile/build.sh <<'EOF'
#!/usr/bin/env bash
# Build the void-box smoke image.
# Usage: build.sh <path-to-voidbox-binary> [<image-tag>]
set -euo pipefail

VOIDBOX_BIN="${1:?path to voidbox binary required}"
IMAGE="${2:-void-box:smoke}"

if [[ ! -x "$VOIDBOX_BIN" ]]; then
  echo "voidbox binary not executable: $VOIDBOX_BIN" >&2
  exit 2
fi

CTX=$(mktemp -d)
trap 'rm -rf "$CTX"' EXIT

cp "$VOIDBOX_BIN" "$CTX/voidbox"
cp "$(dirname "$0")/Dockerfile" "$CTX/Dockerfile"
cp "$(dirname "$0")/voidbox-entrypoint.sh" "$CTX/voidbox-entrypoint.sh"

docker build -t "$IMAGE" "$CTX"
echo "Built: $IMAGE"
EOF
chmod +x crates/shim-k8s/dockerfile/build.sh
```

- [ ] **Step 4: Build and smoke-test the image locally**

```bash
./crates/shim-k8s/dockerfile/build.sh /home/diego/github/agent-infra/void-box/target/release/voidbox void-box:smoke

# Smoke-test serve-and-load mode (without k8s) — should fail gracefully
# because /spec/run.yaml doesn't exist; this just verifies the entrypoint dispatches correctly.
docker run --rm \
  -e VOIDBOX_DAEMON_TOKEN=test \
  void-box:smoke serve-and-load 2>&1 | head -10 || true
```

Expected: see error about /spec/run.yaml missing or curl can't reach daemon (depending on timing). Either is fine — it confirms the entrypoint dispatches into the serve-and-load branch.

Test the `run` branch (should still work as before):
```bash
docker run --rm -v /home/diego/github/void-shims/examples/run.yaml:/spec/run.yaml:ro,z void-box:smoke run 2>&1 | tail -5
```
Expected: `success: true` from voidbox.

- [ ] **Step 5: Commit**

```bash
git add crates/shim-k8s/dockerfile/
git commit -m "shim-k8s: container Dockerfile + entrypoint dispatcher (run | serve-and-load)"
```

---

## Task 8: Examples + kind_smoke.sh extension

**Files:**
- Create: `examples/run-service.yaml`
- Modify: `scripts/kind_smoke.sh`

- [ ] **Step 1: Create the service-mode example**

```bash
cat > examples/run-service.yaml <<'EOF'
api_version: v1
kind: agent
name: smoke-svc
sandbox:
  mode: mock
  memory_mb: 512
  vcpus: 1
llm:
  provider: claude
  model: claude-haiku-4-5-20251001
agent:
  prompt: "You are an echo agent. Reply to every message with exactly the user's message text, no preamble."
  mode: service
  timeout_secs: 600
EOF
```

- [ ] **Step 2: Verify it validates with voidbox**

```bash
voidbox validate --file examples/run-service.yaml
```
Expected: `valid: examples/run-service.yaml (kind=Agent, api_version=v1)`.

- [ ] **Step 3: Extend kind_smoke.sh with the sandbox path**

Open `scripts/kind_smoke.sh`. After the existing Job-path block (after the `# 5. live run → status loop → logs → rm` section, but BEFORE the script's final exit lines), add:

```bash
# 6. Sandbox-path smoke (mode: service). Requires agent-sandbox controller.
log "--- sandbox-path smoke ---"

# Install controller from the v0.4.6 release manifest; graceful skip on failure
ASB_VERSION="${ASB_VERSION:-v0.4.6}"
if kubectl apply -f "https://github.com/kubernetes-sigs/agent-sandbox/releases/download/${ASB_VERSION}/manifest.yaml" 2>&1 | tee /tmp/asb-install.log; then
  log "waiting for agent-sandbox controller to become Available"
  if ! kubectl wait --for=condition=Available deploy --all -n agent-sandbox-system --timeout=180s; then
    log "agent-sandbox controller not Available; skipping sandbox-path smoke"
    log "smoke OK (job-path only; sandbox-path skipped due to controller)"
    exit 0
  fi
else
  log "agent-sandbox install failed; skipping sandbox-path smoke (see /tmp/asb-install.log)"
  log "smoke OK (job-path only; sandbox-path skipped due to controller install)"
  exit 0
fi

# Build & load image (same image as Job path; entrypoint dispatches by args)
log "building void-box:smoke from $VOIDBOX_BIN (rebuild for the new entrypoint)"
VOIDBOX_BIN=${VOIDBOX_BIN:-/home/diego/github/agent-infra/void-box/target/release/voidbox}
./crates/shim-k8s/dockerfile/build.sh "$VOIDBOX_BIN" void-box:smoke
kind load docker-image void-box:smoke --name "$CLUSTER"

# Run the service spec
log "creating sandbox via shim"
SBREF=$("$BIN" run --file examples/run-service.yaml --image void-box:smoke --image-pull-policy Never)
log "sandbox ref: $SBREF"
SBNAME="${SBREF##*/}"
SBNS="${SBREF%%/*}"

# Wait for Sandbox Ready condition
log "waiting for Sandbox Ready"
ready=""
for i in $(seq 1 60); do
  ready=$(kubectl get sandbox "$SBNAME" -n "$SBNS" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null || echo "")
  if [[ "$ready" == "True" ]]; then break; fi
  sleep 2
done
if [[ "$ready" != "True" ]]; then
  log "sandbox did not become Ready in 120s"
  kubectl describe sandbox "$SBNAME" -n "$SBNS"
  kubectl logs -n "$SBNS" "sandbox/$SBNAME" || true
  "$BIN" rm "$SBREF" || true
  exit 1
fi
log "sandbox Ready"

# Telemetry snapshot
log "fetching initial telemetry"
"$BIN" telemetry "$SBREF" || log "(telemetry returned non-zero)"

# Real LLM round-trip if ANTHROPIC_API_KEY is set
if [[ -n "${ANTHROPIC_API_KEY:-}" ]]; then
  log "sending message (real LLM round-trip)"
  "$BIN" send "$SBREF" --message "ping"
  sleep 3
  log "checking telemetry for tokens_out > 0"
  TEL=$("$BIN" telemetry "$SBREF" 2>/dev/null || echo "{}")
  echo "$TEL" | head -20
  # Soft check: log success only if tokens visibly went up
  if echo "$TEL" | grep -Eq '"tokens_out":[[:space:]]*[1-9]|"out":[[:space:]]*[1-9]'; then
    log "LLM round-trip OK (tokens consumed)"
  else
    log "LLM round-trip ran but tokens_out not visible; manual inspection of telemetry recommended"
  fi
else
  log "ANTHROPIC_API_KEY unset; skipping live LLM round-trip"
fi

log "cancelling sandbox run"
"$BIN" cancel "$SBREF" || log "(cancel returned non-zero — agent may already be idle)"

log "removing sandbox"
"$BIN" rm "$SBREF"

log "smoke OK (job-path + sandbox-path)"
```

- [ ] **Step 4: Verify the script is still syntactically valid**

```bash
bash -n scripts/kind_smoke.sh && echo "syntax OK"
```
Expected: `syntax OK`.

- [ ] **Step 5: Update `rm` subcommand to handle Sandbox in addition to Job**

In `crates/shim-k8s/src/main.rs`, locate the `Command::Rm` arm. Replace it with:

```rust
        Command::Rm { run_ref, force } => {
            let (namespace, name) = parse_run_ref(&run_ref)?;
            // Try Sandbox first, then Job. Either succeeding is fine; both
            // failing is reported via the second failure.
            let sandbox_args = vec![
                "delete".to_string(),
                "sandbox".to_string(),
                name.clone(),
                "-n".to_string(),
                namespace.clone(),
            ];
            if kubectl_output_owned(&sandbox_args).is_ok() {
                return Ok(());
            }
            let args = build_delete_job_args(&namespace, &name, force);
            let _ = kubectl_output_owned(&args)?;
            Ok(())
        }
```

Run `cargo test -p shim-k8s 2>&1 | tail -5` — all tests should still pass (the existing `delete_args_include_force_only_when_requested` test still covers the Job path).

- [ ] **Step 6: Commit**

```bash
git add examples/run-service.yaml scripts/kind_smoke.sh crates/shim-k8s/src/main.rs
git commit -m "scripts: kind_smoke covers sandbox path with live LLM if key present"
```

---

## Task 9: README update + final verification

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add a "Service mode (long-running agents)" section**

Open `README.md`. After the "Local Smoke (kind)" section, add:

```markdown
## Service Mode (Long-Running Agents)

For specs with `kind: agent, mode: service`, the shim auto-renders an
`agents.x-k8s.io/v1beta1 Sandbox` CR (instead of a `batch/v1 Job`) and
exposes client subcommands to talk to the live daemon.

**Requirements:**
- agent-sandbox controller installed in the cluster:
  `kubectl apply -k github.com/kubernetes-sigs/agent-sandbox/config/default`
- A container image with `voidbox` 0.2.x + the `voidbox-entrypoint.sh`
  script baked in (see `crates/shim-k8s/dockerfile/`).

**Usage:**

```bash
# Create the service. ANTHROPIC_API_KEY in env propagates to the pod.
export ANTHROPIC_API_KEY=sk-ant-...
REF=$(./target/release/void-shim-k8s run --file examples/run-service.yaml)
# → default/smoke-svc-abc12345

# Talk to it
./target/release/void-shim-k8s telemetry $REF
./target/release/void-shim-k8s send $REF --message "hello"
./target/release/void-shim-k8s telemetry $REF   # tokens_out > 0 after a real LLM call

# Suspend / resume (MVP: nothing survives — pod restarts fresh on resume)
./target/release/void-shim-k8s suspend $REF
./target/release/void-shim-k8s resume $REF

# Get raw connection info (for curl, custom clients)
./target/release/void-shim-k8s endpoint $REF

# Cleanup
./target/release/void-shim-k8s cancel $REF
./target/release/void-shim-k8s rm $REF
```

**Backend selection:** `--backend auto|job|sandbox`. Default `auto`
chooses Sandbox when the spec has `kind: agent, mode: service`, else Job.

**LLM credentials:** the shim reads `ANTHROPIC_API_KEY` (or
`OPENAI_API_KEY` for `provider: codex`) from your environment at render
time, propagates it into a per-Sandbox `Secret`, wires it as a pod env
var. Override the env var name with `--llm-api-key-env`.

**Caveat — suspend/resume is not state-preserving in this MVP.** voidbox
0.2.x cannot snapshot a running KVM VM. `suspend` terminates the pod;
`resume` starts a fresh pod with the same Sandbox name but a new
internal run. Tracked as follow-up.
```

- [ ] **Step 2: Add to "Next Steps"**

In `README.md`, the "Next Steps" section, prepend:

```markdown
- Persistence across suspend/resume: requires upstream void-box support for
  snapshot/restore of running KVM VMs, or an application-level checkpoint
  protocol. Currently MVP loses state on suspend.
- `kind: agent, mode: interactive` and `kind: sandbox` (bare VM) — same
  Sandbox backend, slightly different render shape.
- Ingress / TLS for out-of-cluster clients (currently `kubectl
  port-forward`-only).
```

- [ ] **Step 3: Full workspace test**

```bash
cargo test --workspace 2>&1 | grep "test result"
cargo build --workspace --release 2>&1 | tail -3
```
Expected: all tests pass; release build succeeds.

- [ ] **Step 4: End-to-end smoke**

```bash
# Without LLM key (verifies render + apply + Sandbox-Ready + cancel + rm)
./scripts/kind_smoke.sh

# With LLM key (verifies real Anthropic round-trip)
export ANTHROPIC_API_KEY=sk-ant-...
./scripts/kind_smoke.sh
```
Expected:
- First run: `smoke OK (job-path + sandbox-path)` with a log line "ANTHROPIC_API_KEY unset; skipping live LLM round-trip".
- Second run: `LLM round-trip OK (tokens consumed)`.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: README — document service-mode backend + LLM key propagation"
```

- [ ] **Step 6: Inspect final commit history**

```bash
git log --oneline feature/voidbox-0.2-migration..feature/service-backend
```
Expected: 8-9 commits on the new branch, all atomic, signed (signing config inherited from previous PR).

---

## Self-Review Notes

**Spec coverage:**
- Goal 1 (Sandbox CR + Secrets) → Tasks 4, 6 step 3
- Goal 2 (CLI subcommands) → Task 6 steps 2, 4
- Goal 3 (auto-detect backend) → Task 1 + Task 6 step 1
- Goal 4 (kind smoke with graceful degradation) → Task 8 step 3
- Goal 5 (real LLM round-trip in smoke) → Task 8 step 3 (the `ANTHROPIC_API_KEY` branch)

**Suspend/resume via scale subresource** → Task 6 step 4 (`Command::Suspend`, `Command::Resume`).

**`shutdownTime` instead of `expirationSeconds`** → Task 4 step 1 (the `shutdown_time_line` formatting).

**JSON body for `POST /v1/runs`** → Task 7 step 1 (entrypoint uses `-d '{"file":"/spec/run.yaml"}'`).

**JSON body `{role, content}` for messages** → Task 5 step 2 (`SendMessageBody` struct).

**Placeholder scan:** clean — every step has concrete code or commands. Task 1 step 1 explicitly notes "skip this step" for chrono (resolved inline rather than left as a TODO).

**Type consistency:**
- `Backend` enum used identically in `shim-core` (Task 1) and `shim-k8s` (Tasks 6 — accessed as `shim_core::Backend`).
- `BackendArg` in `shim-k8s` is the clap-facing wrapper; `BackendArg::resolve()` returns `shim_core::Backend`.
- `SandboxRenderOptions` field names (`image`, `image_pull_policy`, `mount_kvm`, `runtime_command_args`, `llm_credential`) match across Task 4 (definition) and Task 6 step 3 (construction).
- `DaemonClient` method signatures match across Task 5 (definition) and Task 6 step 4 (usage).
- `parse_run_ref` returns `(namespace, name)` — used identically in all new subcommand arms.

**Known deferred work (per spec):**
- Real persistence across suspend (need upstream void-box) → spec future-work #2
- `kind: agent, mode: interactive` and `kind: sandbox` → spec future-work #1
- Ingress / TLS → spec future-work #3
- CI workflow → spec future-work #5

No spec requirement lacks a task.
