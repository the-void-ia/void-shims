# void-shims

Integration shims for running **Void-Box** on existing orchestrators.

- `shim-k8s`: Kubernetes Job/Pod runner (MVP target)
- `shim-containerd`: containerd runtime v2 shim (later)
- `shim-libvirt`: libvirt domain adapter (later)

## Spec

See: `spec/shim-v0.1.md`

## Workspace

This repo is a Cargo workspace. Build everything:

```bash
cargo build --workspace
```

Run the Kubernetes shim (render mode):

```bash
cargo run -p shim-k8s -- render --file ./examples/run.yaml > job.yaml
```

Run directly against a cluster (prints `namespace/job_name`):

```bash
cargo run -p shim-k8s -- run --file ./examples/run.yaml
```

Query status, logs, and cleanup:

```bash
cargo run -p shim-k8s -- status default/void-run-abc123
cargo run -p shim-k8s -- logs default/void-run-abc123 --follow
cargo run -p shim-k8s -- rm default/void-run-abc123
```

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

## Notes

- Shims are for *integrations*, not a replacement for `void-control`.
- Shims enforce the **workflow_per_vm** execution profile.

## Next Steps

- Add `--image-pull-secret` support in `shim-k8s` for private GHCR images.
- Add runtime mode for images without `/bin/sh` (direct entrypoint/args support).
- Add a second backend renderer for the `agents.x-k8s.io/v1beta1 Sandbox` CRD.
- GitHub Actions workflow that runs `scripts/kind_smoke.sh` on push.
- Factor `void-box::spec` into a sub-crate so `shim-core` can switch from
  `path` to `git` dependency.
