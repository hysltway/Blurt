- # Claude Code Instructions

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

  During normal implementation, use a debug build for fast compile validation:

  ```powershell
  $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
  Set-Location src-tauri
  cargo build
  ```

  Do not run a release build during intermediate iterations. After implementation and tests are complete, tasks that changed runtime Rust code, Cargo configuration, or `ui/` assets require one final release deployment before reporting the task as done. Documentation-only and repository-instruction-only tasks do not require a build or relaunch.

  ```powershell
  $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
  Set-Location src-tauri
  cargo build --release
  Get-Process Blurt -ErrorAction SilentlyContinue | Stop-Process -Force
  foreach ($i in 1..10) { Start-Sleep -Milliseconds 500; try { Copy-Item target\release\blurt.exe ..\dist\Blurt.exe -Force -ErrorAction Stop; break } catch {} }
  Start-Process ..\dist\Blurt.exe
  ```

  Rebuild notes:

  - `frontendDist` is `../ui`, so HTML/CSS/JS changes only take effect in the rebuilt exe.
  - Run cargo from inside `src-tauri`: the required `+crt-static` rustflags live in `src-tauri/.cargo/config.toml`, and cargo discovers that config by working directory.
  - Do not run `cargo clean` during routine development or deployment. It removes many gigabytes of reusable build artifacts and makes the next build start from scratch. Use it only when the user explicitly requests disk cleanup or when the cache is demonstrably corrupted.
  - Release builds use LTO and consistently take longer than a 120-second command window while linking. Run `cargo build --release` with a timeout of at least 360 seconds. A 120-second timeout by itself is not a build failure; rerun the same build with the longer timeout and reuse the incremental artifacts.
  - Stop Blurt before overwriting `dist\Blurt.exe` (the running exe locks the file). Windows may hold the image lock for a while after Stop-Process, so retry the copy in a loop as shown — a single fixed sleep is not reliable, and starting the old exe after a failed copy leaves a stale build running.
  - Routine deployment is pre-authorized for this repository: stop the running Blurt process, replace `dist\Blurt.exe`, and relaunch it without asking the user for confirmation each time.
  - If the build itself fails linking with `os error 5`, a running Blurt process is locking the target exe — stop it and rerun the build.

  Worktree hygiene:

  - After a `claude/*` branch is merged, remove its worktree right away — every worktree accumulates its own multi-GB `src-tauri/target`.
  - DANGER: `git worktree remove --force` follows NTFS junctions and deletes THROUGH them (it wiped the real `models\` via a worktree junction on 2026-07-27). Worktrees here may contain a `models` junction pointing at `D:\Work\Blurt\models` (created for selftest). Always unlink reparse points first — `.Delete()` removes only the link, never the target:

  ```powershell
  Get-ChildItem -Recurse -Force -Attributes ReparsePoint .claude\worktrees\<worktree-name> | ForEach-Object { $_.Delete() }
  git worktree remove --force .claude\worktrees\<worktree-name>
  ```

  - If removal fails with "Permission denied", clear file attributes first, force-delete the folder, then prune the metadata:

  ```powershell
  attrib -r -s -h ".claude\worktrees\<worktree-name>\*" /s /d
  cmd /c rmdir /s /q ".claude\worktrees\<worktree-name>"
  git worktree prune
  ```

  Do not finish or commit while a required check is failing. If a required tool is unavailable, report that explicitly instead of treating the check as passed.
