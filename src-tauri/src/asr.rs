//! ASR 引擎：sherpa-onnx 加载 Qwen3-ASR-0.6B（int8），常驻内存，热词可配。

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sherpa_onnx::{
    OfflineModelConfig, OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
};

pub struct AsrEngine {
    rec: Mutex<OfflineRecognizer>,
    pub model_dir: PathBuf,
    /// 识别后强制替换规则（热词框里的 `错词=>正词` 条目）
    replaces: Vec<(String, String)>,
}

#[derive(Clone)]
pub enum EngineSlot {
    /// 尚未开始加载 / 正在加载
    Loading,
    Ready(Arc<AsrEngine>),
    /// 未找到模型文件（附期望目录）
    Missing(String),
    Failed(String),
}

fn pick(dir: &Path, names: &[&str]) -> Result<String> {
    for n in names {
        let p = dir.join(n);
        if p.is_file() {
            return Ok(p.to_string_lossy().into_owned());
        }
    }
    Err(anyhow!("缺少模型文件：{:?}", names))
}

impl AsrEngine {
    pub fn load(model_dir: &Path, num_threads: usize, hotwords: &str) -> Result<Self> {
        let threads = if num_threads == 0 {
            (std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                / 2)
            .clamp(2, 8) as i32
        } else {
            num_threads as i32
        };

        // 热词框里每个条目要么是热词，要么是 `错词=>正词` 强制替换规则
        let mut words: Vec<&str> = Vec::new();
        let mut replaces: Vec<(String, String)> = Vec::new();
        for item in hotwords
            .split([',', '，', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match item.split_once("=>") {
                Some((from, to)) if !from.trim().is_empty() && !to.trim().is_empty() => {
                    replaces.push((from.trim().to_string(), to.trim().to_string()));
                }
                _ => words.push(item),
            }
        }
        // Qwen3-ASR 的热词经 sherpa-onnx 注入 system prompt，属软偏置：
        // 裸词表只对与词表同语言的语音有效（实测中文语境下英文裸词表完全不生效）。
        // 用中英双语指令句包裹词表后，中文/英文语境均能把近音词拉回给定拼写。
        // 注意模板不能含 ASCII 逗号（sherpa 侧会按逗号切分后改成空格拼接）。
        let hot = if words.is_empty() {
            String::new()
        } else {
            format!(
                "The following terms may appear and must be transcribed exactly: {}. \
                 以下词条可能出现在语音中、请按给定拼写转写：{}",
                words.join(" "),
                words.join("、"),
            )
        };

        let qwen3 = OfflineQwen3ASRModelConfig {
            conv_frontend: Some(pick(
                model_dir,
                &["conv_frontend.onnx", "conv_frontend.int8.onnx"],
            )?),
            encoder: Some(pick(model_dir, &["encoder.int8.onnx", "encoder.onnx"])?),
            decoder: Some(pick(model_dir, &["decoder.int8.onnx", "decoder.onnx"])?),
            tokenizer: Some(model_dir.join("tokenizer").to_string_lossy().into_owned()),
            hotwords: if hot.is_empty() { None } else { Some(hot) },
            // 长口述需要足够的生成长度（官方 CLI 示例即用 512）
            max_new_tokens: 512,
            ..Default::default()
        };
        let config = OfflineRecognizerConfig {
            model_config: OfflineModelConfig {
                qwen3_asr: qwen3,
                num_threads: threads,
                provider: Some("cpu".into()),
                debug: false,
                ..Default::default()
            },
            ..Default::default()
        };

        let rec = OfflineRecognizer::create(&config)
            .ok_or_else(|| anyhow!("初始化识别器失败（请检查模型文件是否完整）"))?;
        Ok(Self {
            rec: Mutex::new(rec),
            model_dir: model_dir.to_path_buf(),
            replaces,
        })
    }

    /// 输入 16kHz 单声道，返回 (文本, 耗时秒)
    pub fn transcribe(&self, samples: &[f32]) -> Result<(String, f64)> {
        let t0 = Instant::now();
        let rec = self.rec.lock();
        let stream = rec.create_stream();
        stream.accept_waveform(crate::audio::TARGET_SR as i32, samples);
        rec.decode(&stream);
        let mut text = stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .context("读取识别结果失败")?;
        for (from, to) in &self.replaces {
            text = replace_term_ci(&text, from, to);
        }
        Ok((text, t0.elapsed().as_secs_f64()))
    }

    /// 预热：跑一小段静音，完成内存分配与算子初始化
    pub fn warmup(&self) {
        let silence = vec![0.0f32; 8000];
        let _ = self.transcribe(&silence);
    }
}

/// 大小写不敏感（仅 ASCII 折叠）的整词替换。
/// 匹配边界：若规则词首/尾是 ASCII 字母数字，则相邻字符不得是 ASCII 字母数字，
/// 避免 "cloud=>claude" 误伤 "cloudy"；CJK 词条无边界约束，按子串替换。
fn replace_term_ci(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    // 仅折叠 ASCII 大小写：字节长度不变，折叠串的偏移与原串一一对应
    let fold = |s: &str| -> String { s.chars().map(|c| c.to_ascii_lowercase()).collect() };
    let ft = fold(text);
    let ff = fold(from);
    let tb = text.as_bytes();
    let need_left = ff.as_bytes()[0].is_ascii_alphanumeric();
    let need_right = ff.as_bytes()[ff.len() - 1].is_ascii_alphanumeric();

    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    while let Some(off) = ft[pos..].find(&ff) {
        let start = pos + off;
        let end = start + ff.len();
        let left_ok = !need_left || start == 0 || !tb[start - 1].is_ascii_alphanumeric();
        let right_ok = !need_right || end == text.len() || !tb[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            out.push_str(&text[pos..start]);
            out.push_str(to);
            pos = end;
        } else {
            // 折叠串与原串字节对齐，start+1 仍是合法 UTF-8 起点边界之前的位置；
            // 用 ceil 到下一个字符边界避免切在多字节字符中间
            let mut next = start + 1;
            while !text.is_char_boundary(next) {
                next += 1;
            }
            out.push_str(&text[pos..next]);
            pos = next;
        }
    }
    out.push_str(&text[pos..]);
    out
}

/// 以指定线程数一次性 加载→预热→识别，返回识别耗时（毫秒）。
/// 用于「一键测速」量化线程数对速度的影响；引擎用完即弃，不影响常驻实例。
pub fn bench_once(model_dir: &Path, threads: usize, samples: &[f32]) -> Result<f64> {
    let engine = AsrEngine::load(model_dir, threads, "")?;
    engine.warmup();
    let (_, secs) = engine.transcribe(samples)?;
    Ok(secs * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_whole_word_case_insensitive() {
        assert_eq!(
            replace_term_ci("Open Cloud Code and ask cloud.", "cloud", "claude"),
            "Open claude Code and ask claude."
        );
    }

    #[test]
    fn replace_skips_partial_word() {
        assert_eq!(
            replace_term_ci("cloudy cloud clouds", "cloud", "claude"),
            "cloudy claude clouds"
        );
    }

    #[test]
    fn replace_multiword_and_cjk_context() {
        assert_eq!(
            replace_term_ci("用cloud code写代码", "cloud code", "claude code"),
            "用claude code写代码"
        );
        assert_eq!(
            replace_term_ci("打开云端代码", "云端代码", "Claude Code"),
            "打开Claude Code"
        );
    }

    #[test]
    fn replace_handles_multibyte_neighbors_without_panic() {
        // 未命中整词边界时跨多字节字符推进不得 panic
        assert_eq!(replace_term_ci("云cloud云", "云cloud云", "x"), "x");
        assert_eq!(replace_term_ci("abc云", "云", "雲"), "abc雲");
    }
}
