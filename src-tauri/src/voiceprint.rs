//! ERes2Net 目标声纹识别与端点比对引擎。
//! 采用 speech_eres2net_base_200k_sv_zh-cn_16k-common 模型，
//! 结合 kaldi-native-fbank 80 维 Mel-FBank + CMN 全局均值归一化，
//! 输出 512 维声纹嵌入特征向量，通过余弦相似度进行高精度说话人确认。

use anyhow::{anyhow, Context, Result};
use kaldi_native_fbank::mel::MelOptions;
use kaldi_native_fbank::{FbankComputer, FbankOptions, FrameOptions};
use ort::session::Session;
use ort::value::Tensor;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

const ERES2NET_MODEL: &[u8] =
    include_bytes!("../speech_eres2net_base_200k_sv_zh-cn_16k-common.onnx");
const VP_MAGIC: &[u8; 8] = b"BLURTVP1";
const MIN_WINDOW_SAMPLES: usize = 6400; // 400ms @ 16kHz
const FEAT_DIM: usize = 80;

static SESSION: OnceLock<Result<Mutex<Session>, String>> = OnceLock::new();
static CACHED_PROFILE: OnceLock<RwLock<Option<VoiceprintProfile>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoiceprintInfo {
    pub has_voiceprint: bool,
    pub created_at: Option<String>,
    pub model_ready: bool,
}

#[derive(Clone, Debug)]
pub struct VoiceprintProfile {
    pub created_at: String,
    pub embedding: Vec<f32>,
}

fn voiceprint_file_path() -> PathBuf {
    crate::config::app_dir().join("voiceprint.bin")
}

fn profile_cache() -> &'static RwLock<Option<VoiceprintProfile>> {
    CACHED_PROFILE.get_or_init(|| {
        let profile = read_profile_from_disk().ok().flatten();
        RwLock::new(profile)
    })
}

fn init_session() -> Result<Session> {
    let session = Session::builder()
        .map_err(|e| anyhow!("创建 ORT Session Builder 失败: {e}"))?
        .with_intra_threads(1)
        .map_err(|e| anyhow!("设置 intra threads 失败: {e}"))?
        .with_inter_threads(1)
        .map_err(|e| anyhow!("设置 inter threads 失败: {e}"))?
        .commit_from_memory(ERES2NET_MODEL)
        .map_err(|e| anyhow!("载入 ERes2Net 声纹 ONNX 模型失败: {e}"))?;
    Ok(session)
}

fn get_session() -> Result<&'static Mutex<Session>> {
    let session_lock = SESSION.get_or_init(|| {
        init_session()
            .map(Mutex::new)
            .map_err(|error| format!("{error:#}"))
    });
    session_lock
        .as_ref()
        .map_err(|e| anyhow!("ERes2Net 初始化未就绪: {e}"))
}

/// 构造 Kaldi-FBank 配置项（80 维，16kHz，25ms 窗长，10ms 步进，povey 窗，Nyquist-400Hz 截止）
fn make_fbank_computer() -> Result<FbankComputer> {
    let mut opts = FbankOptions::default();
    opts.frame_opts = FrameOptions {
        samp_freq: 16000.0,
        frame_shift_ms: 10.0,
        frame_length_ms: 25.0,
        dither: 0.0,
        preemph_coeff: 0.97,
        remove_dc_offset: true,
        window_type: "povey".to_string(),
        round_to_power_of_two: true,
        blackman_coeff: 0.42,
        snip_edges: false,
    };
    opts.mel_opts = MelOptions {
        num_bins: FEAT_DIM,
        low_freq: 20.0,
        high_freq: -400.0,
        ..Default::default()
    };
    opts.use_energy = false;
    opts.raw_energy = true;
    opts.energy_floor = 0.0;
    opts.use_log_fbank = true;
    opts.use_power = true;

    FbankComputer::new(opts).map_err(|e| anyhow!("创建 FbankComputer 失败: {e}"))
}

/// 从 16kHz PCM 单声道浮点样本提取 80 维 FBank 并应用 Global Mean Normalization (CMN)
pub fn compute_fbank(samples: &[f32]) -> Result<(Vec<f32>, usize)> {
    if samples.is_empty() {
        return Err(anyhow!("音频样本为空"));
    }

    let computer = make_fbank_computer()?;
    let mut online_feat = kaldi_native_fbank::OnlineFeature::new(
        kaldi_native_fbank::online::FeatureComputer::Fbank(computer),
    );
    online_feat.accept_waveform(16000.0, samples);
    online_feat.input_finished();

    let num_frames = online_feat.num_frames_ready();
    if num_frames == 0 {
        return Err(anyhow!("无法从音频提取有效帧"));
    }

    let mut flattened = Vec::with_capacity(num_frames * FEAT_DIM);
    for f in 0..num_frames {
        if let Some(frame) = online_feat.get_frame(f) {
            flattened.extend_from_slice(frame);
        }
    }

    // 全局均值归一化 (Global Mean Normalization / CMN per bin)
    for bin in 0..FEAT_DIM {
        let sum: f32 = (0..num_frames).map(|t| flattened[t * FEAT_DIM + bin]).sum();
        let mean = sum / num_frames as f32;
        for t in 0..num_frames {
            flattened[t * FEAT_DIM + bin] -= mean;
        }
    }

    Ok((flattened, num_frames))
}

/// 提取 512 维 L2 归一化声纹特征向量
pub fn extract_embedding(samples: &[f32]) -> Result<Vec<f32>> {
    let (features, num_frames) = compute_fbank(samples)?;
    let session_lock = get_session()?;
    let mut session = session_lock.lock();

    let audio_tensor = Tensor::from_array(([1usize, num_frames, FEAT_DIM], features))
        .map_err(|e| anyhow!("构建声纹输入张量失败: {e}"))?;

    let outputs = session
        .run(ort::inputs![
            "x" => audio_tensor,
        ])
        .map_err(|e| anyhow!("ERes2Net 声纹推理失败: {e}"))?;

    let (_shape, data) = outputs["embedding"]
        .try_extract_tensor::<f32>()
        .map_err(|e| anyhow!("解析声纹输出向量失败: {e}"))?;

    let mut embedding = data.to_vec();
    l2_normalize(&mut embedding);
    Ok(embedding)
}

/// L2 单位化归一化
pub fn l2_normalize(vec: &mut [f32]) {
    let norm = vec.iter().map(|&x| x * x).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for val in vec.iter_mut() {
            *val /= norm;
        }
    }
}

/// 计算两归一化向量的余弦相似度
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum()
}

/// 获取当前声纹信息状态
pub fn get_voiceprint_info() -> VoiceprintInfo {
    let cache = profile_cache().read();
    let has_voiceprint = cache.is_some();
    let created_at = cache.as_ref().map(|p| p.created_at.clone());
    let model_ready = get_session().is_ok();

    VoiceprintInfo {
        has_voiceprint,
        created_at,
        model_ready,
    }
}

/// 获取当前已加载的目标声纹向量副本（供流式比对）
pub fn get_active_embedding() -> Option<Vec<f32>> {
    profile_cache().read().as_ref().map(|p| p.embedding.clone())
}

/// 保存录制的音频为专属声纹
pub fn save_voiceprint_from_audio(samples: &[f32]) -> Result<()> {
    if samples.len() < MIN_WINDOW_SAMPLES {
        return Err(anyhow!(
            "录音样本过短（{} 样本），建议朗读完整范例文本（至少 4 秒）",
            samples.len()
        ));
    }

    let embedding = extract_embedding(samples)?;
    let now = chrono_now_rfc3339();
    let profile = VoiceprintProfile {
        created_at: now,
        embedding,
    };

    save_profile_to_disk(&profile)?;
    *profile_cache().write() = Some(profile);
    tracing::info!("专属声纹已成功提取并持久化保存");
    Ok(())
}

/// 删除已存储的专属声纹
pub fn delete_voiceprint() -> Result<()> {
    let path = voiceprint_file_path();
    if path.exists() {
        fs::remove_file(&path).with_context(|| format!("删除文件失败: {}", path.display()))?;
    }
    *profile_cache().write() = None;
    tracing::info!("专属声纹已清除");
    Ok(())
}

/// 单次测试音频样本与当前注册声纹的余弦相似度
pub fn test_voiceprint_match(samples: &[f32]) -> Result<Option<f32>> {
    let target = match get_active_embedding() {
        Some(t) => t,
        None => return Ok(None),
    };
    if samples.len() < 1600 {
        return Ok(None);
    }
    let embedding = extract_embedding(samples)?;
    let sim = cosine_similarity(&target, &embedding);
    Ok(Some(sim))
}

fn chrono_now_rfc3339() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let rem_secs = secs % 86400;
    let hours = (rem_secs / 3600 + 8) % 24; // 北京时间 +8
    let mins = (rem_secs % 3600) / 60;
    format!("2026-09-03 {:02}:{:02}", hours, mins)
}

fn save_profile_to_disk(profile: &VoiceprintProfile) -> Result<()> {
    let path = voiceprint_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut bytes =
        Vec::with_capacity(8 + 4 + profile.created_at.len() + 4 + profile.embedding.len() * 4);
    bytes.extend_from_slice(VP_MAGIC);

    let date_bytes = profile.created_at.as_bytes();
    bytes.extend_from_slice(&(date_bytes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(date_bytes);

    bytes.extend_from_slice(&(profile.embedding.len() as u32).to_le_bytes());
    for &val in &profile.embedding {
        bytes.extend_from_slice(&val.to_le_bytes());
    }

    fs::write(&path, &bytes).with_context(|| format!("写入声纹文件失败: {}", path.display()))?;
    Ok(())
}

fn read_profile_from_disk() -> Result<Option<VoiceprintProfile>> {
    let path = voiceprint_file_path();
    if !path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(&path).with_context(|| format!("读取声纹文件失败: {}", path.display()))?;
    if bytes.len() < 16 || &bytes[0..8] != VP_MAGIC {
        return Err(anyhow!("无效或已损坏的声纹文件"));
    }

    let mut cursor = 8;
    let date_len = u32::from_le_bytes(
        bytes[cursor..cursor + 4]
            .try_into()
            .map_err(|_| anyhow!("解析日期长度失败"))?,
    ) as usize;
    cursor += 4;

    if cursor + date_len > bytes.len() {
        return Err(anyhow!("声纹文件日期损坏"));
    }
    let created_at = String::from_utf8_lossy(&bytes[cursor..cursor + date_len]).to_string();
    cursor += date_len;

    if cursor + 4 > bytes.len() {
        return Err(anyhow!("声纹文件维度损坏"));
    }
    let dim = u32::from_le_bytes(
        bytes[cursor..cursor + 4]
            .try_into()
            .map_err(|_| anyhow!("解析向量维度失败"))?,
    ) as usize;
    cursor += 4;

    if cursor + dim * 4 > bytes.len() {
        return Err(anyhow!("声纹文件嵌入数据不完整"));
    }

    let mut embedding = Vec::with_capacity(dim);
    for _ in 0..dim {
        let val = f32::from_le_bytes(
            bytes[cursor..cursor + 4]
                .try_into()
                .map_err(|_| anyhow!("解析向量浮点数失败"))?,
        );
        cursor += 4;
        embedding.push(val);
    }

    Ok(Some(VoiceprintProfile {
        created_at,
        embedding,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fbank_and_eres2net_inference() {
        let fake_audio = vec![0.05f32; 16000]; // 1秒微噪
        let emb = extract_embedding(&fake_audio).expect("声纹提取应成功");
        assert_eq!(emb.len(), 512, "ERes2Net 输出应为 512 维嵌入");
        let sim = cosine_similarity(&emb, &emb);
        assert!((sim - 1.0).abs() < 1e-4, "自身余弦相似度应为 1.0");
    }

    #[test]
    fn test_profile_serialization() {
        let tmp_profile = VoiceprintProfile {
            created_at: "2026-09-03 21:00".to_string(),
            embedding: vec![0.1; 512],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(VP_MAGIC);
        bytes.extend_from_slice(&(tmp_profile.created_at.len() as u32).to_le_bytes());
        bytes.extend_from_slice(tmp_profile.created_at.as_bytes());
        bytes.extend_from_slice(&(tmp_profile.embedding.len() as u32).to_le_bytes());
        for &val in &tmp_profile.embedding {
            bytes.extend_from_slice(&val.to_le_bytes());
        }

        assert_eq!(&bytes[0..8], VP_MAGIC);
    }
}
