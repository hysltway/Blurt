# Development Rules

## Required validation

- After changing Rust code, run `cargo fmt --manifest-path src-tauri/Cargo.toml --all`.
- Then run `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check` to verify that no formatting changes remain.
- Run `cargo test --manifest-path src-tauri/Cargo.toml` after Rust code or Cargo configuration changes.
- For HTML, CSS, JavaScript, PowerShell, JSON, and TOML changes, preserve the existing style and run any applicable formatter or validator configured in the repository.
- Fix validation failures and rerun the failed command before considering the work complete. Never report a check as passing unless it was actually run successfully.

## Scope

- Keep changes focused on the requested task.
- Do not commit generated files from `src-tauri/target`, `src-tauri/gen`, or model files from `models`.
- Preserve unrelated user changes in the working tree.
