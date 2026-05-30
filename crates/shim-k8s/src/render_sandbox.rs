//! Renders agents.x-k8s.io/v1alpha1 Sandbox CR + auxiliary Secrets.

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{Duration, Utc};
use shim_core::{RunSpec, ShimError};

/// Rendering options for a Sandbox CR.
///
/// Mirrors the role of `RenderOptions` for the Job backend: image, pull policy,
/// whether to mount `/dev/kvm` + privileged, the entrypoint args, and an optional
/// LLM credential to render as a sibling Secret.
#[derive(Debug, Clone)]
pub struct SandboxRenderOptions {
    pub image: String,
    pub image_pull_policy: String,
    pub mount_kvm: bool,
    /// Arguments passed to the container's entrypoint script (typically
    /// `["serve-and-load"]` for sandbox mode).
    pub runtime_command_args: Vec<String>,
    /// If [`Some`], an LLM credentials Secret is rendered with this (env-var, value) pair.
    pub llm_credential: Option<(String, String)>,
}

/// Renders 3 or 4 YAML documents joined by `"\n---\n"`:
/// 1. Sandbox CR
/// 2. Token Secret (always)
/// 3. Spec Secret (always)
/// 4. LLM credentials Secret (only when `options.llm_credential` is [`Some`])
///
/// All operator-controlled scalar inputs (namespace, sandbox_name, image,
/// pull policy, runtime args, LLM env-var name) are validated against
/// newline injection before interpolation; an invalid input returns
/// [`ShimError::InvalidMount`] (the variant is shared with mount validation).
pub fn render_sandbox_yaml(
    namespace: &str,
    sandbox_name: &str,
    options: &SandboxRenderOptions,
    spec: &RunSpec,
    run_id: uuid::Uuid,
    token: &str,
) -> Result<String, ShimError> {
    validate_yaml_scalar(namespace, "namespace")?;
    validate_yaml_scalar(sandbox_name, "sandbox_name")?;
    validate_yaml_scalar(&options.image, "image")?;
    validate_yaml_scalar(&options.image_pull_policy, "image_pull_policy")?;
    for arg in &options.runtime_command_args {
        validate_yaml_scalar(arg, "runtime_command_args")?;
        if arg.contains('"') {
            return Err(ShimError::InvalidMount(format!(
                "runtime_command_args entries must not contain double-quote, got: {arg:?}"
            )));
        }
    }
    if let Some((env_var, _)) = &options.llm_credential {
        validate_yaml_scalar(env_var, "llm_credential env_var")?;
    }

    let run_yaml = serde_yaml::to_string(spec)?;
    let run_yaml_b64 = BASE64_STANDARD.encode(run_yaml.as_bytes());
    let token_b64 = BASE64_STANDARD.encode(token.as_bytes());

    let shutdown_time_line = match spec.agent.as_ref().and_then(|a| a.timeout_secs) {
        Some(secs) => {
            let when = Utc::now() + Duration::seconds(secs as i64);
            // v1alpha1 puts shutdownTime/shutdownPolicy at top-level of .spec
            // (not nested under `lifecycle:` as v1beta1 will).
            format!(
                "  shutdownTime: \"{}\"\n  shutdownPolicy: Delete\n",
                when.to_rfc3339()
            )
        }
        None => String::new(),
    };

    let privileged_yaml = if options.mount_kvm {
        "          securityContext:\n            privileged: true\n"
    } else {
        ""
    };

    let llm_env = match &options.llm_credential {
        Some((env_var, _)) => format!(
            "            - name: {env_var}\n              valueFrom:\n                secretKeyRef:\n                  name: {sandbox_name}-llm-creds\n                  key: {env_var}\n"
        ),
        None => String::new(),
    };

    let devkvm_volume_mount = if options.mount_kvm {
        "            - name: devkvm\n              mountPath: /dev/kvm\n".to_string()
    } else {
        String::new()
    };
    let devkvm_volume = if options.mount_kvm {
        "        - name: devkvm\n          hostPath:\n            path: /dev/kvm\n            type: CharDevice\n".to_string()
    } else {
        String::new()
    };

    // Render spec.sandbox.mounts[] as hostPath volumes (same pattern as Job
    // renderer). Without this, host mounts declared in the spec disappear in
    // service mode and agents expecting workspace/config mounts break.
    let mut extra_volume_mounts = String::new();
    let mut extra_volumes = String::new();
    for (i, m) in spec.sandbox.mounts.iter().enumerate() {
        if !m.host.starts_with('/') {
            return Err(ShimError::InvalidMount(format!(
                "mount host path must be absolute, got: {}",
                m.host
            )));
        }
        if m.host.contains('\n') || m.host.contains('\r') {
            return Err(ShimError::InvalidMount(format!(
                "mount host path must not contain newlines, got: {:?}",
                m.host
            )));
        }
        let name = format!("mount-{i}");
        let read_only = m.mode == "ro";
        extra_volume_mounts.push_str(&format!(
            "            - name: {name}\n              mountPath: {path}\n              readOnly: {ro}\n",
            name = name, path = m.host, ro = read_only
        ));
        extra_volumes.push_str(&format!(
            "        - name: {name}\n          hostPath:\n            path: {path}\n            type: DirectoryOrCreate\n",
            name = name, path = m.host
        ));
    }
    let devkvm_volume_mount = format!("{devkvm_volume_mount}{extra_volume_mounts}");
    let devkvm_volume = format!("{devkvm_volume}{extra_volumes}");

    let mut args_yaml = String::new();
    for arg in &options.runtime_command_args {
        args_yaml.push_str(&format!("            - \"{arg}\"\n"));
    }

    let sandbox_doc = format!(
        r#"apiVersion: agents.x-k8s.io/v1alpha1
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
{args_yaml}          env:
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
            exec:
              command:
                - /bin/sh
                - -c
                - 'curl -fsf -H "Authorization: Bearer $VOIDBOX_DAEMON_TOKEN" http://127.0.0.1:43100/v1/runs >/dev/null'
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

/// Validates that a string is safe to interpolate into a YAML scalar position.
///
/// Rejects newlines (CR/LF) which would break the manifest structure. Mirrors
/// the helper in `main.rs` for the Job backend; duplicated here to avoid a
/// cross-module dependency that Task 6 will refactor when it wires Sandbox
/// rendering into the CLI.
fn validate_yaml_scalar(value: &str, field: &str) -> Result<(), ShimError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(ShimError::InvalidScalar(
            field.to_string(),
            format!("must not contain newlines, got: {value:?}"),
        ));
    }
    Ok(())
}

/// Generates a fresh 32-byte random token as 64 hex chars.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use shim_core::parse_spec_str;

    fn sample_service_spec() -> RunSpec {
        parse_spec_str(
            r#"
api_version: v1
kind: agent
name: test
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
llm: { provider: claude }
agent: { prompt: "hi", mode: service, timeout_secs: 60 }
"#,
        )
        .unwrap()
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
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(yaml.contains("kind: Sandbox"), "{yaml}");
        assert!(yaml.contains("replicas: 1"), "{yaml}");
        assert!(yaml.contains("service: true"), "{yaml}");
    }

    #[test]
    fn render_includes_token_secret_ref_and_env() {
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(yaml.contains("name: VOIDBOX_DAEMON_TOKEN"), "{yaml}");
        assert!(yaml.contains("name: t-svc-token"), "{yaml}");
        assert!(yaml.contains("kind: Secret"), "{yaml}");
    }

    #[test]
    fn render_includes_readiness_probe_with_bearer_auth() {
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(yaml.contains("readinessProbe:"), "{yaml}");
        assert!(yaml.contains("/v1/runs"), "{yaml}");
        // exec probe expands $VOIDBOX_DAEMON_TOKEN via /bin/sh; k8s does NOT
        // expand $(VAR) inside httpHeaders.value, so httpGet would always 401.
        assert!(yaml.contains("Bearer $VOIDBOX_DAEMON_TOKEN"), "{yaml}");
    }

    #[test]
    fn render_includes_kvm_for_mount_kvm_true() {
        let mut opts = default_options();
        opts.mount_kvm = true;
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &opts,
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(yaml.contains("privileged: true"), "{yaml}");
        assert!(yaml.contains("/dev/kvm"), "{yaml}");
    }

    #[test]
    fn render_excludes_kvm_for_mount_kvm_false() {
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(!yaml.contains("privileged: true"), "{yaml}");
        assert!(!yaml.contains("/dev/kvm"), "{yaml}");
    }

    #[test]
    fn render_emits_three_docs_without_llm_creds() {
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(
            docs.len(),
            3,
            "expected 3 docs; got {}:\n{}",
            docs.len(),
            yaml
        );
    }

    #[test]
    fn render_emits_four_docs_with_llm_creds() {
        let mut opts = default_options();
        opts.llm_credential = Some(("ANTHROPIC_API_KEY".into(), "sk-test".into()));
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &opts,
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        let docs: Vec<&str> = yaml.split("\n---\n").collect();
        assert_eq!(
            docs.len(),
            4,
            "expected 4 docs; got {}:\n{}",
            docs.len(),
            yaml
        );
        assert!(yaml.contains("name: t-svc-llm-creds"), "{yaml}");
        // base64 of "sk-test" is "c2stdGVzdA=="
        assert!(yaml.contains("ANTHROPIC_API_KEY: c2stdGVzdA=="), "{yaml}");
    }

    #[test]
    fn render_includes_shutdown_time_when_timeout_set() {
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(yaml.contains("shutdownTime:"), "{yaml}");
        assert!(yaml.contains("shutdownPolicy: Delete"), "{yaml}");
        // v1alpha1: top-level under spec, NOT nested under `lifecycle:`
        assert!(
            !yaml.contains("lifecycle:"),
            "v1alpha1 has no lifecycle: nesting; {yaml}"
        );
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
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &spec,
            uuid::Uuid::nil(),
            "tok",
        )
        .unwrap();
        assert!(!yaml.contains("shutdownTime:"), "{yaml}");
    }

    #[test]
    fn render_rejects_namespace_with_newline() {
        let err = render_sandbox_yaml(
            "ns\ninject",
            "t-svc",
            &default_options(),
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .expect_err("newline in namespace should fail");
        assert!(matches!(err, ShimError::InvalidScalar(_, _)), "got {err:?}");
    }

    #[test]
    fn render_rejects_runtime_arg_with_quote() {
        let mut opts = default_options();
        opts.runtime_command_args = vec!["bad\"arg".into()];
        let err = render_sandbox_yaml(
            "default",
            "t-svc",
            &opts,
            &sample_service_spec(),
            uuid::Uuid::nil(),
            "tok",
        )
        .expect_err("quote in runtime arg should fail");
        // runtime arg validation still uses InvalidMount (kept for now)
        assert!(matches!(err, ShimError::InvalidMount(_)), "got {err:?}");
    }

    #[test]
    fn render_emits_volumes_for_sandbox_mounts() {
        let mut spec = sample_service_spec();
        spec.sandbox.mounts = vec![
            shim_core::MountSpec {
                host: "/tmp/a".into(),
                guest: "/a".into(),
                mode: "ro".into(),
            },
            shim_core::MountSpec {
                host: "/tmp/b".into(),
                guest: "/b".into(),
                mode: "rw".into(),
            },
        ];
        let yaml = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &spec,
            uuid::Uuid::nil(),
            "tok",
        )
        .expect("render ok");
        assert!(
            yaml.contains("name: mount-0"),
            "mount-0 missing in:\n{yaml}"
        );
        assert!(
            yaml.contains("name: mount-1"),
            "mount-1 missing in:\n{yaml}"
        );
        assert!(yaml.contains("path: /tmp/a"), "/tmp/a missing in:\n{yaml}");
        assert!(
            yaml.contains("readOnly: true"),
            "readOnly true missing in:\n{yaml}"
        );
        assert!(
            yaml.contains("readOnly: false"),
            "readOnly false missing in:\n{yaml}"
        );
    }

    #[test]
    fn render_sandbox_rejects_relative_mount_host() {
        let mut spec = sample_service_spec();
        spec.sandbox.mounts = vec![shim_core::MountSpec {
            host: "rel/path".into(),
            guest: "/a".into(),
            mode: "ro".into(),
        }];
        let err = render_sandbox_yaml(
            "default",
            "t-svc",
            &default_options(),
            &spec,
            uuid::Uuid::nil(),
            "tok",
        )
        .expect_err("relative path should fail");
        assert!(matches!(err, ShimError::InvalidMount(_)), "got {err:?}");
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
