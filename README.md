# Blurt

**完全离线的 Windows 语音输入工具，为 vibe coding 而生。**

按住 Ctrl+Alt，说出想法，准确的中英混合文本立刻落进 Claude Code、VS Code 或终端的光标处。全程零联网——语音与文本永远不离开这台电脑。

---

## 设计哲学：让 3 秒等待毫无焦虑

本地识别存在 2–3 秒延迟，这是客观事实。Blurt 不试图掩盖它，而是让这段时间**全程可感知**：

| 阶段 | HUD 动画（全程无文字） | 传达的信息 |
|------|------------------------|------------|
| 按下 Ctrl+Alt | 胶囊弹出，声波条随你的音量起伏 | **“它听到我了”** |
| 松开按键 | 声波条聚成圆点行波，底部进度线按预测时长推进 | **“正在处理，还要这么久”** |
| 完成 | 圆点聚合、绿色脉冲、文字落进光标 | **“好了”** |
| 出错 / 无语音 / 取消 | 红色抖动 / 灰色塌陷 / 快速消散 | 各有明确形态 |

进度线的时长预测来自**历史识别速度的滑动平均（RTF EMA）**，越用越准。

## 使用

- **按住说话**（≥350ms）：按住 `Ctrl + Alt`，说完松开，文字即出 —— 对讲机式
- **轻点切换**（<350ms）：点一下开始，再点一下结束 —— 适合长段口述
- **Esc**：录音或识别过程中随时取消
- 快捷键固定为 `Ctrl + Alt`（纯修饰键组合，`RegisterHotKey` 注册不了，
  Blurt 用常驻低级键盘钩子实现；按住期间再按第三个键会视为普通快捷键并自动取消录音）
- 托盘双击打开设置：注入方式、麦克风、热词、开机自启……

**热词**：把 `Claude, Tauri, rebase, PR` 这类易错专有名词填进设置，识别准确率显著提升。

## 安装

1. 安装 Blurt（或直接运行 `blurt.exe`）
2. 下载识别模型（约 937 MB，一次性）：

   ```powershell
   pwsh -File scripts/get-model.ps1
   ```

   模型默认放到 `%APPDATA%\Blurt\models`；也可指定目录（如项目内）：
   `pwsh -File scripts/get-model.ps1 -Dest D:\Work\Blurt\models`。
   亦可手动下载
   [sherpa-onnx-qwen3-asr-0.6B-int8](https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-qwen3-asr-0.6B-int8-2026-03-25.tar.bz2)
   解压到上述任一目录。

   > 模型查找顺序：设置中指定的目录 → exe 所在目录及其上级的 `models\` → `%APPDATA%\Blurt\models`。
3. 启动 Blurt，等托盘提示「就绪」，按住 Ctrl+Alt 开说。

> 模型加载后常驻内存（约 1.5 GB），识别速度约为实时的 6–10 倍：
> 说 10 秒话 ≈ 1–2 秒出字。

## 技术

- **识别**：[Qwen3-ASR-0.6B](https://github.com/QwenLM/Qwen3-ASR)（int8 量化），
  经 [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) 本地推理，中英混说、31 种语言、自动标点
- **框架**：Tauri 2（Rust），静态前端，无 Node 依赖
- **注入**：短文本模拟键入（`SendInput` Unicode），长文本剪贴板粘贴并自动恢复原剪贴板；
  绝不注入回车，不会误触发送
- **隐私**：运行期无任何网络请求

## 从源码构建

需要：Rust（MSVC 工具链）、VS Build Tools（C++）、WebView2（Win11 自带）

```powershell
pwsh -File scripts/make-icons.ps1        # 生成图标
cd src-tauri
cargo build --release                    # 产物 target/release/blurt.exe
cargo run -- --selftest                  # 端到端自检（模型加载→识别→打印结果）
```

打包安装程序（NSIS）：`cargo tauri build`

## 常见问题

- **文字没出来？** 目标窗口若以管理员运行，需要 Blurt 也以管理员运行（Windows UIPI 限制）。注入失败时文本会兜底放进剪贴板，直接 Ctrl+V 即可。
- **快捷键没反应？** 查看托盘悬停提示是否报「键盘监听」错误；个别安全软件会拦截低级键盘钩子，放行 Blurt 即可。
- **想更快？** 设置里把推理线程调到 6–8（默认自动）。
