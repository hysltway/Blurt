//! 配置与持久化：%APPDATA%\Blurt\{config.json, state.json, logs, models}

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 全局快捷键：修饰键小写 + W3C 键码，例如 "ctrl+alt+Space"
    pub hotkey: String,
    /// 注入方式：auto | type | paste
    pub inject_mode: String,
    /// auto 模式下，长度 ≤ 该值用模拟键入，否则用剪贴板粘贴
    pub type_threshold: usize,
    /// 麦克风设备名，None = 系统默认
    pub mic_device: Option<String>,
    /// 推理线程数，0 = 自动
    pub num_threads: usize,
    /// 热词，逗号分隔
    pub hotwords: String,
    /// 最长录音秒数，超时自动开始识别
    pub max_record_secs: u64,
    /// 开机自启动
    pub autostart: bool,
    /// 模型目录覆盖，None = 自动探测
    pub model_dir: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey: "ctrl+alt+Space".into(),
            inject_mode: "auto".into(),
            type_threshold: 20,
            mic_device: None,
            num_threads: 0,
            hotwords: String::new(),
            max_record_secs: 120,
            autostart: false,
            model_dir: None,
        }
    }
}

pub fn app_dir() -> PathBuf {
    let d = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Blurt");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn logs_dir() -> PathBuf {
    let d = app_dir().join("logs");
    let _ = fs::create_dir_all(&d);
    d
}

pub fn models_root() -> PathBuf {
    let d = app_dir().join("models");
    let _ = fs::create_dir_all(&d);
    d
}

fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

fn state_path() -> PathBuf {
    app_dir().join("state.json")
}

pub fn load() -> Config {
    let p = config_path();
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => {
            let c = Config::default();
            let _ = save(&c);
            c
        }
    }
}

pub fn save(c: &Config) -> anyhow::Result<()> {
    fs::write(config_path(), serde_json::to_string_pretty(c)?)?;
    Ok(())
}

/// 运行期统计（用于 HUD 进度预测），跨会话持久化
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// 识别耗时 / 音频时长 的滑动平均
    pub rtf_ema: f32,
    /// 最近一次识别耗时（毫秒）
    pub last_ms: Option<u64>,
}

impl Default for Stats {
    fn default() -> Self {
        Self { rtf_ema: 0.16, last_ms: None }
    }
}

pub fn load_stats() -> Stats {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_stats(s: &Stats) {
    if let Ok(j) = serde_json::to_string(s) {
        let _ = fs::write(state_path(), j);
    }
}

/// 一个目录是否为有效的 Qwen3-ASR 模型目录
pub fn is_model_dir(d: &std::path::Path) -> bool {
    let has = |names: &[&str]| names.iter().any(|n| d.join(n).is_file());
    has(&["conv_frontend.onnx", "conv_frontend.int8.onnx"])
        && has(&["encoder.int8.onnx", "encoder.onnx"])
        && has(&["decoder.int8.onnx", "decoder.onnx"])
        && d.join("tokenizer").join("vocab.json").is_file()
}

/// 解析模型目录：配置指定 → %APPDATA%\Blurt\models\* → exe 同级 models\*
pub fn resolve_model_dir(cfg: &Config) -> Option<PathBuf> {
    if let Some(m) = &cfg.model_dir {
        let p = PathBuf::from(m);
        if is_model_dir(&p) {
            return Some(p);
        }
    }
    let mut roots = vec![models_root()];
    if let Ok(exe) = std::env::current_exe() {
        // exe 同级及向上数级的 models\：覆盖 dist\、target\release\ 与项目根目录布局
        for dir in exe.ancestors().skip(1).take(4) {
            roots.push(dir.join("models"));
        }
    }
    for root in roots {
        if is_model_dir(&root) {
            return Some(root);
        }
        if let Ok(entries) = fs::read_dir(&root) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && is_model_dir(p))
                .collect();
            dirs.sort();
            if let Some(d) = dirs.pop() {
                return Some(d);
            }
        }
    }
    None
}
