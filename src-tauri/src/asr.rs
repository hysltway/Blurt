//! ASR 引擎：sherpa-onnx 加载 Qwen3-ASR-0.6B（int8），常驻内存，热词可配。

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sherpa_onnx::{
    OfflineModelConfig, OfflineQwen3ASRModelConfig, OfflineRecognizer, OfflineRecognizerConfig,
};

const SPLIT_TRIGGER_S: f32 = 28.0;
const MAX_CHUNK_S: f32 = 24.0;
const MIN_CHUNK_S: f32 = 8.0;
const CUT_SEARCH_S: f32 = 2.5;
const FORCED_OVERLAP_S: f32 = 0.3;
const CUT_FRAME: usize = 320; // 20ms @16kHz
const CUT_SMOOTH_FRAMES: usize = 5;
const MAX_DEDUP_CHARS: usize = 24;

#[derive(Clone, Debug, PartialEq, Eq)]
struct AudioChunk {
    start: usize,
    end: usize,
    overlap_with_previous: bool,
}

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
        let chunks = plan_audio_chunks(samples);
        let rec = self.rec.lock();
        if chunks.len() > 1 {
            let durations = chunks
                .iter()
                .map(|chunk| {
                    format!(
                        "{:.2}s{}",
                        (chunk.end - chunk.start) as f32 / crate::audio::TARGET_SR as f32,
                        if chunk.overlap_with_previous {
                            "+重叠"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" / ");
            tracing::info!(
                "长音频分段识别：{:.2}s -> {} 段（{}）",
                samples.len() as f32 / crate::audio::TARGET_SR as f32,
                chunks.len(),
                durations
            );
        }

        let mut text = String::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let chunk_t0 = Instant::now();
            let part = decode_chunk(&rec, &samples[chunk.start..chunk.end])
                .with_context(|| format!("识别长音频片段 {}/{} 失败", i + 1, chunks.len()))?;
            if chunks.len() > 1 {
                tracing::info!(
                    "长音频片段 {}/{} 完成 {:.2}s（音频 {:.2}s，输出 {} 字）",
                    i + 1,
                    chunks.len(),
                    chunk_t0.elapsed().as_secs_f64(),
                    (chunk.end - chunk.start) as f32 / crate::audio::TARGET_SR as f32,
                    part.chars().count()
                );
            }
            append_chunk_text(&mut text, &part, chunk.overlap_with_previous);
        }

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

fn decode_chunk(rec: &OfflineRecognizer, samples: &[f32]) -> Result<String> {
    let stream = rec.create_stream();
    stream.accept_waveform(crate::audio::TARGET_SR as i32, samples);
    rec.decode(&stream);
    stream
        .get_result()
        .map(|r| r.text.trim().to_string())
        .context("读取识别结果失败")
}

fn plan_audio_chunks(samples: &[f32]) -> Vec<AudioChunk> {
    let sr = crate::audio::TARGET_SR as usize;
    let split_trigger = (SPLIT_TRIGGER_S * sr as f32) as usize;
    if samples.len() <= split_trigger {
        return vec![AudioChunk {
            start: 0,
            end: samples.len(),
            overlap_with_previous: false,
        }];
    }

    let max_chunk = (MAX_CHUNK_S * sr as f32) as usize;
    let min_chunk = (MIN_CHUNK_S * sr as f32) as usize;
    let search = (CUT_SEARCH_S * sr as f32) as usize;
    let overlap = (FORCED_OVERLAP_S * sr as f32) as usize;
    let chunk_count = samples.len().div_ceil(max_chunk);

    let energies: Vec<f32> = samples
        .chunks(CUT_FRAME)
        .map(|frame| {
            let sum: f64 = frame
                .iter()
                .map(|&sample| sample as f64 * sample as f64)
                .sum();
            (sum / frame.len() as f64).sqrt() as f32
        })
        .collect();
    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[sorted.len() / 10];
    let median = sorted[sorted.len() / 2];
    let quiet_threshold = (floor * 1.8).min(median * 0.45).max(0.004);

    let mut boundaries: Vec<(usize, bool)> = Vec::with_capacity(chunk_count - 1);
    let mut previous_cut = 0usize;
    for chunks_left in (2..=chunk_count).rev() {
        let remaining = samples.len() - previous_cut;
        let remaining_after = chunks_left - 1;
        let hard_min = (previous_cut + min_chunk)
            .max(samples.len().saturating_sub(remaining_after * max_chunk));
        let hard_max = (previous_cut + max_chunk)
            .min(samples.len().saturating_sub(remaining_after * min_chunk));
        let ideal = (previous_cut + remaining / chunks_left).clamp(hard_min, hard_max);
        let search_min = hard_min.max(ideal.saturating_sub(search));
        let search_max = hard_max.min(ideal.saturating_add(search));
        let (cut, quiet) = choose_cut(&energies, ideal, search_min, search_max, quiet_threshold);
        boundaries.push((cut, quiet));
        previous_cut = cut;
    }

    let mut chunks: Vec<AudioChunk> = Vec::with_capacity(chunk_count);
    let mut start = 0usize;
    for (cut, quiet) in boundaries {
        let overlap_with_previous = chunks.last().is_some_and(|previous| start < previous.end);
        chunks.push(AudioChunk {
            start,
            end: cut,
            overlap_with_previous,
        });
        start = if quiet {
            cut
        } else {
            cut.saturating_sub(overlap)
        };
    }
    let overlap_with_previous = chunks.last().is_some_and(|previous| start < previous.end);
    chunks.push(AudioChunk {
        start,
        end: samples.len(),
        overlap_with_previous,
    });
    chunks
}

fn choose_cut(
    energies: &[f32],
    ideal_sample: usize,
    min_sample: usize,
    max_sample: usize,
    quiet_threshold: f32,
) -> (usize, bool) {
    let first_frame = min_sample.div_ceil(CUT_FRAME).max(1);
    let last_frame = (max_sample / CUT_FRAME).min(energies.len().saturating_sub(1));
    let ideal_frame = ideal_sample / CUT_FRAME;

    let candidates: Vec<(usize, f32)> = (first_frame..=last_frame)
        .map(|frame| (frame, smoothed_energy(energies, frame)))
        .collect();
    if let Some(&(frame, _)) = candidates
        .iter()
        .filter(|(_, energy)| *energy <= quiet_threshold)
        .min_by(|(a_frame, a_energy), (b_frame, b_energy)| {
            a_frame
                .abs_diff(ideal_frame)
                .cmp(&b_frame.abs_diff(ideal_frame))
                .then_with(|| a_energy.total_cmp(b_energy))
        })
    {
        return (frame * CUT_FRAME, true);
    }

    let frame = candidates
        .iter()
        .min_by(|(a_frame, a_energy), (b_frame, b_energy)| {
            a_energy.total_cmp(b_energy).then_with(|| {
                a_frame
                    .abs_diff(ideal_frame)
                    .cmp(&b_frame.abs_diff(ideal_frame))
            })
        })
        .map_or(ideal_frame, |(frame, _)| *frame);
    ((frame * CUT_FRAME).clamp(min_sample, max_sample), false)
}

fn smoothed_energy(energies: &[f32], center: usize) -> f32 {
    let radius = CUT_SMOOTH_FRAMES / 2;
    let start = center.saturating_sub(radius);
    let end = (center + radius + 1).min(energies.len());
    energies[start..end].iter().sum::<f32>() / (end - start) as f32
}

fn append_chunk_text(combined: &mut String, part: &str, overlapped: bool) {
    let mut part = part.trim();
    if part.is_empty() {
        return;
    }
    if combined.is_empty() {
        combined.push_str(part);
        return;
    }

    if overlapped {
        if let Some(prefix_end) = overlap_prefix_end(combined, part) {
            part = part[prefix_end..].trim_start();
            if part.is_empty() {
                return;
            }
        }
    }

    let left = combined.chars().next_back();
    let right = part.chars().next();
    if left.is_some_and(|c| c.is_ascii_alphanumeric())
        && right.is_some_and(|c| c.is_ascii_alphanumeric())
    {
        combined.push(' ');
    }
    combined.push_str(part);
}

fn overlap_prefix_end(left: &str, right: &str) -> Option<usize> {
    let left_chars = significant_chars(left);
    let right_chars = significant_chars(right);
    let max = left_chars.len().min(right_chars.len()).min(MAX_DEDUP_CHARS);

    for len in (2..=max).rev() {
        let left_start = left_chars.len() - len;
        if left_chars[left_start..]
            .iter()
            .map(|(c, _)| c)
            .eq(right_chars[..len].iter().map(|(c, _)| c))
        {
            return Some(right_chars[len - 1].1);
        }
    }
    None
}

fn significant_chars(text: &str) -> Vec<(char, usize)> {
    text.char_indices()
        .filter_map(|(start, c)| {
            (!is_merge_punctuation(c)).then_some((c.to_ascii_lowercase(), start + c.len_utf8()))
        })
        .collect()
}

fn is_merge_punctuation(c: char) -> bool {
    c.is_whitespace()
        || c.is_ascii_punctuation()
        || matches!(
            c,
            '，' | '。' | '！' | '？' | '；' | '：' | '、' | '“' | '”' | '‘' | '’'
        )
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

    #[test]
    fn long_audio_prefers_silent_balanced_boundaries() {
        let sr = crate::audio::TARGET_SR as usize;
        let samples = vec![0.0f32; ((SPLIT_TRIGGER_S + 2.0) as usize) * sr];
        let chunks = plan_audio_chunks(&samples);

        assert_eq!(chunks.len(), 2);
        assert!(!chunks[1].overlap_with_previous);
        assert_eq!(chunks[0].end, chunks[1].start);
        assert_eq!(chunks[0].start, 0);
        assert_eq!(chunks[1].end, samples.len());
        assert!(chunks.iter().all(|chunk| {
            chunk.end - chunk.start <= (MAX_CHUNK_S * crate::audio::TARGET_SR as f32) as usize
        }));
    }

    #[test]
    fn continuous_audio_uses_small_overlap_at_forced_boundary() {
        let sr = crate::audio::TARGET_SR as usize;
        let samples = vec![0.1f32; ((SPLIT_TRIGGER_S + 2.0) as usize) * sr];
        let chunks = plan_audio_chunks(&samples);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[1].overlap_with_previous);
        assert!(chunks[1].start < chunks[0].end);
        assert!(chunks[0].end - chunks[1].start <= (FORCED_OVERLAP_S * sr as f32) as usize);
    }

    #[test]
    fn overlapped_text_is_deduplicated_without_breaking_punctuation() {
        let mut combined = "前半段然后".to_string();
        append_chunk_text(&mut combined, "然后，继续", true);
        assert_eq!(combined, "前半段然后，继续");

        let mut english = "hello".to_string();
        append_chunk_text(&mut english, "world", false);
        assert_eq!(english, "hello world");
    }
}
