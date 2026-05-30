//! Thin re-export of void-box's spec types, plus shim-specific error type.

pub use void_box::spec::{
    AgentMode, AgentSpec, LlmSpec, MountSpec, ObserveSpec, PipelineSpec, RunKind, RunSpec,
    SandboxSpec, WorkflowSpec,
};

/// Identifies which Kubernetes primitive the shim should render for a given [`RunSpec`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `batch/v1 Job` — for run-to-completion specs (workflow, agent task, pipeline).
    Job,
    /// `agents.x-k8s.io/v1alpha1 Sandbox` — for long-running agent services.
    Sandbox,
}

/// Selects the rendering backend for a void-box spec.
///
/// Returns [`Backend::Sandbox`] for `kind: agent` with `agent.mode: service`.
/// Returns [`Backend::Job`] for every other shape.
pub fn detect_backend(spec: &RunSpec) -> Backend {
    let RunKind::Agent = spec.kind else {
        return Backend::Job;
    };
    let Some(agent) = spec.agent.as_ref() else {
        return Backend::Job;
    };
    let AgentMode::Service = agent.mode else {
        return Backend::Job;
    };
    Backend::Sandbox
}

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
    #[error("yaml parse: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    /// A required CRD is not installed in the cluster.
    #[error("missing CRD: {0}")]
    MissingCrd(String),
    /// A user-controlled string that would break YAML structure (newlines, etc).
    /// Used for non-mount scalar inputs (namespace, image, command, etc).
    #[error("invalid {0}: {1}")]
    InvalidScalar(String, String),
    /// The voidbox daemon returned an error or unreachable.
    #[error("daemon http error: {0}")]
    DaemonHttp(String),
    /// `kubectl port-forward` failed to bind or terminate.
    #[error("port-forward failed: {0}")]
    PortForward(String),
    /// A subcommand was invoked against a `run_ref` of the wrong backend.
    #[error("backend mismatch: run_ref {0} is backend {1}, command requires {2}")]
    BackendMismatch(String, String, String),
    /// The daemon's `/v1/runs` list did not contain exactly one run.
    #[error("voidbox run not found (expected 1 active run, got {0}); run may have completed or been cancelled")]
    DaemonRunNotFound(usize),
}

/// Parse + validate a void-box RunSpec from a file path.
pub fn load_spec_for_shim(path: &std::path::Path) -> Result<RunSpec, ShimError> {
    Ok(void_box::spec::load_spec(path)?)
}

/// Parse a void-box RunSpec from a YAML string without running validation.
/// Use in tests; production callers must use `load_spec_for_shim`.
pub fn parse_spec_str(s: &str) -> Result<RunSpec, ShimError> {
    Ok(serde_yaml::from_str(s)?)
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
        assert!(matches!(err, ShimError::Yaml(_)));
    }

    #[test]
    fn detect_backend_agent_service_is_sandbox() {
        let spec = parse_spec_str(
            r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", mode: service, timeout_secs: 60 }
"#,
        )
        .unwrap();
        assert_eq!(detect_backend(&spec), Backend::Sandbox);
    }

    #[test]
    fn detect_backend_agent_task_is_job() {
        let spec = parse_spec_str(
            r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", mode: task }
"#,
        )
        .unwrap();
        assert_eq!(detect_backend(&spec), Backend::Job);
    }

    #[test]
    fn detect_backend_agent_interactive_is_job_for_now() {
        let spec = parse_spec_str(
            r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi", mode: interactive }
"#,
        )
        .unwrap();
        assert_eq!(detect_backend(&spec), Backend::Job);
    }

    #[test]
    fn detect_backend_workflow_is_job() {
        let spec = parse_spec_str(
            r#"
api_version: v1
kind: workflow
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
workflow:
  steps:
    - name: s1
      run: { program: echo, args: ["hi"] }
"#,
        )
        .unwrap();
        assert_eq!(detect_backend(&spec), Backend::Job);
    }

    #[test]
    fn detect_backend_agent_no_mode_defaults_to_job() {
        let spec = parse_spec_str(
            r#"
api_version: v1
kind: agent
name: t
sandbox: { mode: mock, memory_mb: 256, vcpus: 1 }
agent: { prompt: "hi" }
"#,
        )
        .unwrap();
        assert_eq!(detect_backend(&spec), Backend::Job);
    }
}
