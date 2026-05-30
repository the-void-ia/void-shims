//! Pre-flight: verify required CRDs are installed.

use shim_core::ShimError;
use std::process::Command;

/// Returns `Ok(())` if the agent-sandbox CRD is installed; `Err(MissingCrd)` otherwise.
///
/// Shells out to `kubectl get crd sandboxes.agents.x-k8s.io`. Network failures
/// and other errors bubble up as `Err(MissingCrd)` with the underlying message
/// for easier diagnosis.
pub fn check_sandbox_crd_installed() -> Result<(), ShimError> {
    let output = Command::new("kubectl")
        .args(["get", "crd", "sandboxes.agents.x-k8s.io", "-o", "name"])
        .output()
        .map_err(|e| ShimError::MissingCrd(format!("could not invoke kubectl: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ShimError::MissingCrd(format!(
            "sandboxes.agents.x-k8s.io CRD not found in cluster. \
             Install with: kubectl apply -f https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v0.4.6/manifest.yaml\n\
             (kubectl said: {})",
            stderr.trim()
        )));
    }
    Ok(())
}
