# Agent 仓库开发规范

## 1. 代码格式化与测试（必须执行）

每次修改 Rust 代码后必须执行格式化：

```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
```

修改 Rust 代码或 Cargo 配置后必须执行测试：

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
```

## 2. 编译与部署流程

- **日常调试验证**（仅调试编译，不发布）：
```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location src-tauri
cargo build
```

- **最终发布部署**（修改 Rust、Cargo 或 `ui/` 代码后必须执行并拉起软件；纯文档任务无需部署）：
```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
Set-Location src-tauri
cargo build --profile release-fast
Get-Process Blurt, blurt -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
foreach ($i in 1..10) { Start-Sleep -Milliseconds 500; try { Copy-Item target\release-fast\blurt.exe ..\dist\Blurt.exe -Force -ErrorAction Stop; break } catch {} }
Start-Process -FilePath "..\dist\Blurt.exe" -WorkingDirectory "..\dist"
Start-Sleep -Seconds 1
Get-Process Blurt, blurt -ErrorAction SilentlyContinue
```

## 3. 构建与进程管理要点

- **路径要求**：必须在 `src-tauri` 目录下执行 cargo（以正确加载 `+crt-static` 静态链接配置）。
- **静态资源**：前端资源目录为 `ui/`，界面改动需重新编译打包后方能生效。
- **禁止清理缓存**：日常严禁执行 `cargo clean`。
- **构建 Profile**：日常部署使用 `release-fast`（禁用 LTO，保留增量缓存）。
- **预授权部署**：日常部署无需向用户确认，直接关闭旧进程、覆盖 `dist\Blurt.exe` 并重新拉起。
- **拉起与防卡死规范**：
  1. 覆盖后必须启动并使用 `Get-Process` 确认进程存活，禁止构建完不拉起。
  2. 拉起 `Blurt.exe` 严禁带 `-Wait` 参数或管道重定向，必须作为独立后台 GUI 进程运行。
  3. 关闭旧进程后必须预留 `Start-Sleep -Seconds 1` 等待单实例命名管道完全释放，防止新进程误判秒退。
  4. 严禁残留阻塞式后台任务。
- **链接锁文件处理**：若构建报 `os error 5`，说明旧进程未完全退出并锁定了 exe，需强制关闭进程后重试。

## 4. 工作区（Worktree）清理

分支合并后立即删除对应工作区，删除时必须先解除软链接/挂载点（ReparsePoint）以防级联删除源文件：

```powershell
Get-ChildItem -Recurse -Force -Attributes ReparsePoint .claude\worktrees\<worktree-name> | ForEach-Object { $_.Delete() }
git worktree remove --force .claude\worktrees\<worktree-name>
```

若提示权限被拒：
```powershell
attrib -r -s -h ".claude\worktrees\<worktree-name>\*" /s /d
cmd /c rmdir /s /q ".claude\worktrees\<worktree-name>"
git worktree prune
```

所有检查必须真实通过，严禁在检查失败或工具不可用时谎报通过。
