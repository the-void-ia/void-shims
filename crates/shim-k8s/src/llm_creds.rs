//! Map void-box LLM provider to env vars + render credentials Secret.

use shim_core::RunSpec;

/// Returns the env var name the daemon expects for the given LLM provider.
///
/// Returns `None` if the provider does not need credentials (e.g. `ollama`,
/// `lm-studio`) or is unrecognized.
pub fn env_var_for_provider(provider: &str) -> Option<&'static str> {
    match provider.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => Some("ANTHROPIC_API_KEY"),
        "codex" | "openai" => Some("OPENAI_API_KEY"),
        "ollama" | "lm-studio" | "lm_studio" => None,
        _ => None,
    }
}

/// Resolves the LLM credential the operator's environment provides for a spec.
///
/// Returns `Some((env_var, value))` when the spec declares an LLM provider that
/// needs an API key AND the corresponding env var is set on the operator's
/// machine. Returns `None` otherwise. The `override_env_var` argument forces
/// a specific env var name regardless of provider.
pub fn resolve_llm_credential(
    spec: &RunSpec,
    override_env_var: Option<&str>,
) -> Option<(String, String)> {
    let llm = spec.llm.as_ref()?;
    let env_var: String = match override_env_var {
        Some(v) => v.to_string(),
        None => env_var_for_provider(&llm.provider)?.to_string(),
    };
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
        // SAFETY: tests run with unique env-var names per test to avoid
        // process-global mutation races; this var is only touched here.
        unsafe { std::env::remove_var("VOID_SHIMS_TEST_LLM_UNSET") };
        let spec = spec_with_provider("claude");
        let got = resolve_llm_credential(&spec, Some("VOID_SHIMS_TEST_LLM_UNSET"));
        assert_eq!(got, None);
    }

    #[test]
    fn resolve_returns_some_when_env_set() {
        // SAFETY: unique env-var name; not touched by any other test.
        unsafe { std::env::set_var("VOID_SHIMS_TEST_LLM_SET", "sk-ant-test-12345") };
        let spec = spec_with_provider("anthropic");
        let got = resolve_llm_credential(&spec, Some("VOID_SHIMS_TEST_LLM_SET"));
        assert_eq!(
            got,
            Some(("VOID_SHIMS_TEST_LLM_SET".into(), "sk-ant-test-12345".into()))
        );
        unsafe { std::env::remove_var("VOID_SHIMS_TEST_LLM_SET") };
    }

    #[test]
    fn resolve_respects_override_env_var() {
        // SAFETY: unique env-var name; not touched by any other test.
        unsafe { std::env::set_var("VOID_SHIMS_TEST_LLM_OVERRIDE", "value-xyz") };
        let spec = spec_with_provider("claude");
        let got = resolve_llm_credential(&spec, Some("VOID_SHIMS_TEST_LLM_OVERRIDE"));
        assert_eq!(
            got,
            Some(("VOID_SHIMS_TEST_LLM_OVERRIDE".into(), "value-xyz".into()))
        );
        unsafe { std::env::remove_var("VOID_SHIMS_TEST_LLM_OVERRIDE") };
    }
}
