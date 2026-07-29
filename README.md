# Blurt

**基于豆包 API 的 Windows 语音输入工具，为 vibe coding 而生。**

按住 Ctrl+Alt，说出想法，中英混合文本会落进 Claude Code、VS Code 或终端的光标处。Blurt 只保留豆包流式语音识别，不需要下载或加载本地 ASR 模型。

---

## 交互

| 阶段 | HUD 动画（全程无文字） | 传达的信息 |
|------|------------------------|------------|
| 按下 Ctrl+Alt | 胶囊弹出，声波条随音量起伏 | **正在聆听** |
| 松开按键 | 声波条聚成圆点行波，底部进度线推进 | **正在处理** |
| 完成 | 圆点聚合、绿色脉冲、文字落进光标 | **识别完成** |
| 出错 / 无语音 / 取消 | 红色抖动 / 灰色塌陷 / 快速消散 | 明确区分结果 |

- **按住说话**（>=350ms）：按住 `Ctrl + Alt`，说完松开。
- **轻点切换**（<350ms）：点一下开始，再点一下结束，适合长段口述。
- **Esc**：录音或识别过程中随时取消。
- 快捷键固定为 `Ctrl + Alt`；按住期间再按第三个键会视为普通快捷键并取消录音。
- 托盘双击打开设置，可配置注入方式、麦克风、热词和开机自启。

## 安装

1. 安装 Blurt，或直接运行 `Blurt.exe`。
2. 从托盘打开设置，输入豆包语音服务的 API Key。密钥只保存在 Windows 凭据管理器。
3. 托盘提示“就绪（豆包 API）”后，按住 `Ctrl + Alt` 开始说话。

无需下载 Qwen、ONNX 或其他本地识别模型。

## 技术

- **识别**：豆包流式语音识别 1.0，录音期间按 200 ms 分包实时发送。
- **自动暂停**：[FunASR FSMN-VAD](https://huggingface.co/funasr/fsmn-vad)（Apache-2.0）在本机检测语音结束。内置模型约 1.7 MB，只做端点检测，不做语音转写。
- **框架**：Tauri 2（Rust），静态前端，无 Node 依赖。
- **注入**：短文本模拟键入，长文本剪贴板粘贴并恢复原剪贴板；不会注入回车。
- **隐私**：录音会发送到豆包语音服务；API Key 不写入 `config.json` 或日志。

## 从源码构建

需要 Rust（MSVC 工具链）、VS Build Tools（C++）和 WebView2（Windows 11 自带）。

```powershell
pwsh -File scripts/make-icons.ps1
cd src-tauri
cargo test
cargo build
```

日常开发使用 `cargo build`，保留增量缓存可显著缩短后续编译。实现和测试全部完成后，再执行最终发布构建：

```powershell
cd src-tauri
cargo build --release
```

发布产物为 `src-tauri\target\release\blurt.exe`。打包安装程序可运行 `cargo tauri build`。

不要在日常开发或部署后例行运行 `cargo clean`。它能释放数 GB 空间，但会让下一次构建从零开始；仅在明确需要回收磁盘空间或缓存损坏时使用：

```powershell
cd src-tauri
cargo clean
```

## 常见问题

- **文字没出来？** 目标窗口若以管理员运行，Blurt 也需要以管理员身份运行。注入失败时文本会放进剪贴板，可直接 Ctrl+V。
- **快捷键没反应？** 查看托盘提示是否有键盘监听错误；个别安全软件会拦截低级键盘钩子。
- **提示缺少密钥？** 打开设置重新保存 API Key，确认对应豆包语音服务账号可用。
