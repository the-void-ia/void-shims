# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust workspace for integration shims that run Void-Box on external orchestrators.

- `crates/shim-core`: shared types, validation, and error handling (`RunSpec`, `ShimError`).
- `crates/shim-k8s`: current MVP CLI/binary (`void-shim-k8s`) for rendering/running Kubernetes jobs.
- `crates/shim-containerd`, `crates/shim-libvirt`: placeholder crates for future adapters.
- `examples/run.yaml`: sample RunSpec input.
- `spec/shim-v0.1.md`: protocol/behavior spec for all shims.

Keep shim-specific logic inside its crate and move reusable types into `shim-core`.

## Build, Test, and Development Commands
Use workspace-level commands from the repository root:

- `cargo build --workspace`: compile all crates.
- `cargo test --workspace`: run all tests.
- `cargo fmt --all`: format Rust code.
- `cargo clippy --workspace --all-targets -- -D warnings`: lint and fail on warnings.
- `cargo run -p shim-k8s -- render --file ./examples/run.yaml > job.yaml`: render a Kubernetes Job manifest from a RunSpec.

Run `fmt` and `clippy` before opening a PR.

## Coding Style & Naming Conventions
- Follow Rust 2021 idioms and default `rustfmt` formatting (4-space indentation, trailing commas where appropriate).
- Use `snake_case` for functions/modules/files, `PascalCase` for types/enums, and `SCREAMING_SNAKE_CASE` for constants.
- Prefer explicit error propagation (`Result<_, ShimError>`) over panics.
- Keep CLI surface consistent with spec verbs: `run`, `status`, `logs`, `rm`, `render`.

## Testing Guidelines
- Place unit tests alongside modules (`#[cfg(test)] mod tests`) and integration tests in `crates/<crate>/tests/` when behavior spans modules.
- Test parsing/validation paths in `shim-core` and CLI/render behavior in `shim-k8s`.
- Name tests by behavior, e.g., `rejects_empty_workflow_file`.
- Run `cargo test --workspace` locally before commit.

## Commit & Pull Request Guidelines
No commit history exists yet, so use this convention going forward:

- Commit format: `type(scope): imperative summary` (example: `feat(shim-k8s): add render namespace flag`).
- Keep commits focused; separate refactors from behavior changes.
- PRs should include: purpose, key changes, test/lint results, and any spec impact (`spec/shim-v0.1.md`).
- For CLI/output changes, include a short example command and resulting snippet.

## Security & Configuration Notes
- Shims must enforce `execution.isolation_mode = workflow_per_vm` regardless of user input.
- Do not introduce silent fallbacks for declared mounts or policy checks.

## Current Next Steps
- Add Kubernetes image pull secret support for private registry workflows.
- Support shell-less runtime images by allowing explicit container command/args wiring.
- Implement `RunSpec.mounts` to Kubernetes volume translation in `shim-k8s`.
- Add e2e Minikube smoke automation and capture it in CI once kubectl/minikube are available.
- Extend status/reporting to include pull failures and container termination reasons.
