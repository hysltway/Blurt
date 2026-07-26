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

        let hot = hotwords
            .split([',', '，', '\n'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(",");

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
        })
    }

    /// 输入 16kHz 单声道，返回 (文本, 耗时秒)
    pub fn transcribe(&self, samples: &[f32]) -> Result<(String, f64)> {
        let t0 = Instant::now();
        let rec = self.rec.lock();
        let stream = rec.create_stream();
        stream.accept_waveform(crate::audio::TARGET_SR as i32, samples);
        rec.decode(&stream);
        let text = stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .context("读取识别结果失败")?;
        Ok((text, t0.elapsed().as_secs_f64()))
    }

    /// 预热：跑一小段静音，完成内存分配与算子初始化
    pub fn warmup(&self) {
        let silence = vec![0.0f32; 8000];
        let _ = self.transcribe(&silence);
    }
}

/// 以指定线程数一次性 加载→预热→识别，返回识别耗时（毫秒）。
/// 用于「一键测速」量化线程数对速度的影响；引擎用完即弃，不影响常驻实例。
pub fn bench_once(model_dir: &Path, threads: usize, samples: &[f32]) -> Result<f64> {
    let engine = AsrEngine::load(model_dir, threads, "")?;
    engine.warmup();
    let (_, secs) = engine.transcribe(samples)?;
    Ok(secs * 1000.0)
}
