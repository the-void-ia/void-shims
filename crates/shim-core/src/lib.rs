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
