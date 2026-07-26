# Claude Code Instructions

Follow all repository rules in `AGENTS.md`.

After every Rust code change, formatting is mandatory:

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
```

After Rust code or Cargo configuration changes, also run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
```

Do not finish or commit while a required check is failing. If a required tool is unavailable, report that explicitly instead of treating the check as passed.
