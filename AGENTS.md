# Agent 仓库开发规范

## 1. 代码格式化与测试

- **修改 Rust 代码后必须执行格式化**：
```powershell
cargo fmt --manifest-path src-tauri/Cargo.toml --all
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
```

- **测试执行策略**：
  - **修改 Rust 代码或 Cargo 配置**：必须执行 `cargo test --manifest-path src-tauri/Cargo.toml`
  - **纯 UI (`ui/`) 或文档修改**：**严禁执行 `cargo test`**（避免触发 Debug 依赖的全量重复编译）。

## 2. 编译加速与资源限制

- **全局共享缓存**：执行前必须设置 `$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo\target_shared\blurt"`，实现跨 Worktree 依赖秒级复用。
- **并发线程限制**：所有 cargo 构建命令必须带 `-j 4`，防止打满 CPU/内存。
- **路径与 Profile**：必须在 `src-tauri` 目录下执行；日常部署使用 `--profile release-fast`。严禁执行 `cargo clean`。

## 3. 发布部署与进程管理

修改 Rust、Cargo 或 `ui/` 代码后必须执行编译、部署并拉起软件：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo\target_shared\blurt"
Set-Location src-tauri
cargo build --profile release-fast -j 4

# 关闭旧进程并预留 2 秒释放 WebView2 与单实例命名管道
Get-Process Blurt, blurt -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# 覆盖目标文件
foreach ($i in 1..10) { try { Copy-Item "$env:CARGO_TARGET_DIR\release-fast\blurt.exe" ..\dist\Blurt.exe -Force -ErrorAction Stop; break } catch { Start-Sleep -Milliseconds 500 } }

# 脱离终端作业树拉起独立 GUI 进程
$distExe = (Resolve-Path "..\dist\Blurt.exe").Path
Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "`"$distExe`"" }
Start-Sleep -Seconds 2

# 二次触发以在前台唤出设置窗口
Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "`"$distExe`"" }
Start-Sleep -Seconds 1
Get-Process Blurt, blurt -ErrorAction SilentlyContinue | Format-Table -AutoSize
```

- **拉起与进程规范**：
  1. 必须使用 `Invoke-CimMethod`（Win32_Process）拉起，禁止依赖可能被终端 Job Object 回收的命令。
  2. 关闭旧进程后必须预留 2 秒等待 WebView2 缓存与单实例管道完全释放，防止新进程报错秒退。
  3. 部署后必须使用 `Get-Process` 确认进程存活。

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

## 5. Git 提交与文件操作规范

- **严禁擅自 Git 提交**：只有在用户明确发出提交指令后方可执行 `git commit`，严禁在未得到明确要求的情况下自行创建提交。
- **严禁擅自恢复已删除文件**：对于已经删除的文件，严禁自行执行恢复操作（如 `git restore`、`git checkout` 或重新创建），除非用户明确要求恢复。

所有检查必须真实通过，严禁在检查失败或工具不可用时谎报通过。
