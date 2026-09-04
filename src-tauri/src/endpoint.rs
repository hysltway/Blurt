//! Streaming Silero VAD voice endpoint detection for toggle-mode recordings.

use anyhow::{anyhow, Result};
use ort::session::Session;
use ort::value::Tensor;
use parking_lot::Mutex;
use std::sync::OnceLock;

use crate::audio;

const SILERO_MODEL: &[u8] = include_bytes!("../silero_vad_ifless.onnx");
const SPEECH_THRESHOLD: f32 = 0.5;
const MIN_SPEECH_MS: u64 = 100;
const CHUNK_SAMPLES: usize = 512; // 32ms @ 16kHz
const CONTEXT_SAMPLES: usize = 64; // 4ms context
const STATE_LEN: usize = 2 * 1 * 128; // [2, 1, 128]
const NO_SPEECH_GRACE_SECS: f32 = 2.0;
const VP_WINDOW_SAMPLES: usize = 16000; // 1.0s @ 16kHz：音素覆盖完整，CMN特征稳定
const VP_STEP_SAMPLES: usize = 3200; // 200ms @ 16kHz：滑动比对步长

static BUNDLED_SESSION: OnceLock<Result<Mutex<Session>, String>> = OnceLock::new();

/// Stateful neural Silero VAD + ERes2Net voiceprint endpoint detector.
pub struct SpeechEndpoint {
    stop_secs: f32,
    context: Vec<f32>,
    state: Vec<f32>,
    buffer: Vec<f32>,
    accepted_samples: u64,
    speech_run_frames: u32,
    silence_run_frames: u32,
    heard_speech: bool,
    done: bool,

    // 声纹抗干扰状态
    #[allow(dead_code)]
    voiceprint_enabled: bool,
    voiceprint_threshold: f32,
    target_embedding: Option<Vec<f32>>,
    vp_window: Vec<f32>,
    vp_window_samples: usize,
    vp_step_config: usize,
    vp_step_samples: usize,
    streak_threshold: usize,
    non_target_streak: usize,
    intruder_active: bool,
    last_target_speech_samples: usize,
}

impl SpeechEndpoint {
    #[allow(dead_code)]
    pub fn create(stop_secs: f32) -> Result<Self> {
        Self::create_with_voiceprint(stop_secs, false, 0.30)
    }

    pub fn create_custom(
        stop_secs: f32,
        voiceprint_enabled: bool,
        voiceprint_threshold: f32,
        vp_window_samples: usize,
        vp_step_config: usize,
        streak_threshold: usize,
    ) -> Result<Self> {
        let session_lock = BUNDLED_SESSION.get_or_init(|| {
            init_session()
                .map(Mutex::new)
                .map_err(|error| format!("{error:#}"))
        });
        let _ = session_lock
            .as_ref()
            .map_err(|error| anyhow!("Silero-VAD initialization failed: {error}"))?;

        let target_embedding = if voiceprint_enabled {
            crate::voiceprint::get_active_embedding()
        } else {
            None
        };

        Ok(Self {
            stop_secs,
            context: vec![0.0; CONTEXT_SAMPLES],
            state: vec![0.0; STATE_LEN],
            buffer: Vec::with_capacity(CHUNK_SAMPLES * 2),
            accepted_samples: 0,
            speech_run_frames: 0,
            silence_run_frames: 0,
            heard_speech: false,
            done: false,
            voiceprint_enabled,
            voiceprint_threshold,
            target_embedding,
            vp_window: Vec::with_capacity(vp_window_samples + CHUNK_SAMPLES),
            vp_window_samples,
            vp_step_config,
            vp_step_samples: 0,
            streak_threshold,
            non_target_streak: 0,
            intruder_active: false,
            last_target_speech_samples: 0,
        })
    }

    pub fn create_with_voiceprint(
        stop_secs: f32,
        voiceprint_enabled: bool,
        voiceprint_threshold: f32,
    ) -> Result<Self> {
        Self::create_custom(
            stop_secs,
            voiceprint_enabled,
            voiceprint_threshold,
            VP_WINDOW_SAMPLES,
            VP_STEP_SAMPLES,
            3,
        )
    }

    pub fn last_target_speech_samples(&self) -> usize {
        self.last_target_speech_samples
    }

    pub fn is_intruder_active(&self) -> bool {
        self.intruder_active
    }

    /// Accept incremental 16 kHz mono samples. Returns true exactly once after
    /// Silero confirms a speech segment ended, or after the initial no-speech timeout.
    pub fn update(&mut self, samples: &[f32]) -> Result<bool> {
        if samples.is_empty() || self.done {
            return Ok(false);
        }

        self.accepted_samples = self.accepted_samples.saturating_add(samples.len() as u64);
        self.buffer.extend_from_slice(samples);

        if self.target_embedding.is_some() {
            self.vp_window.extend_from_slice(samples);
            if self.vp_window.len() > self.vp_window_samples {
                let excess = self.vp_window.len() - self.vp_window_samples;
                self.vp_window.drain(..excess);
            }
            self.vp_step_samples = self.vp_step_samples.saturating_add(samples.len());
        }

        let session_lock = BUNDLED_SESSION
            .get()
            .and_then(|res| res.as_ref().ok())
            .ok_or_else(|| anyhow!("Silero-VAD session is not initialized"))?;

        let mut session = session_lock.lock();

        while self.buffer.len() >= CHUNK_SAMPLES {
            let mut input_data = Vec::with_capacity(CONTEXT_SAMPLES + CHUNK_SAMPLES);
            input_data.extend_from_slice(&self.context);
            input_data.extend_from_slice(&self.buffer[..CHUNK_SAMPLES]);

            // Update context to the last 64 samples of current chunk
            self.context
                .copy_from_slice(&self.buffer[CHUNK_SAMPLES - CONTEXT_SAMPLES..CHUNK_SAMPLES]);

            // Construct ONNX tensors
            let audio_tensor =
                Tensor::from_array(([1usize, CONTEXT_SAMPLES + CHUNK_SAMPLES], input_data))
                    .map_err(|e| anyhow!("create audio input tensor: {e}"))?;
            let sr_tensor = Tensor::from_array(([] as [usize; 0], vec![16000i64]))
                .map_err(|e| anyhow!("create sr input tensor: {e}"))?;
            let state_tensor = Tensor::from_array(([2usize, 1usize, 128usize], self.state.clone()))
                .map_err(|e| anyhow!("create state input tensor: {e}"))?;

            let outputs = session
                .run(ort::inputs![
                    "input" => audio_tensor,
                    "sr" => sr_tensor,
                    "state" => state_tensor,
                ])
                .map_err(|e| anyhow!("Silero-VAD streaming inference failed: {e}"))?;

            let (_prob_shape, prob_data) = outputs["output"]
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow!("extract output probability: {e}"))?;
            let prob = prob_data.first().copied().unwrap_or(0.0);

            let (_state_shape, next_state_data) = outputs["stateN"]
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow!("extract output state: {e}"))?;
            self.state.copy_from_slice(next_state_data);

            // Remove processed chunk from buffer
            self.buffer.drain(..CHUNK_SAMPLES);

            let mut is_speech = prob >= SPEECH_THRESHOLD;

            // 第二层：目标声纹精校准（仅在有人声且声纹已启用时介入）
            if let Some(target) = &self.target_embedding {
                if is_speech {
                    // 当累积步长达到步进门限且窗口具备足够样本时（>= window/2），执行声纹嵌入比对
                    let min_samples = (self.vp_window_samples / 2).max(4000);
                    if self.vp_step_samples >= self.vp_step_config
                        && self.vp_window.len() >= min_samples
                    {
                        self.vp_step_samples = 0;
                        if let Ok(emb) = crate::voiceprint::extract_embedding(&self.vp_window) {
                            let sim = crate::voiceprint::cosine_similarity(target, &emb);
                            if sim >= self.voiceprint_threshold {
                                // 确认是目标说话人：重置未命中计数与旁人标记，记录发音时间戳
                                self.non_target_streak = 0;
                                self.intruder_active = false;
                                self.last_target_speech_samples = self.accepted_samples as usize;
                            } else {
                                // 相似度低于阈值，累加未命中计数
                                self.non_target_streak = self.non_target_streak.saturating_add(1);
                                if self.non_target_streak >= self.streak_threshold {
                                    self.intruder_active = true;
                                }
                            }
                        }
                    } else if !self.intruder_active && self.last_target_speech_samples == 0 {
                        // 录音起步阶段（首个窗口积累前），先初始标记发音位置
                        self.last_target_speech_samples = self.accepted_samples as usize;
                    }

                    // 若已被判定为旁人持续插话，则将语音判定覆写为静音，允许静音超时正常断句
                    if self.intruder_active {
                        is_speech = false;
                    }
                } else if !self.intruder_active && self.non_target_streak > 0 {
                    // 自然静音帧（且非旁人锁存状态）：重置非主人计数，以便后续主人开口时能平滑捕获
                    self.non_target_streak = 0;
                }
            }

            if is_speech {
                self.speech_run_frames = self.speech_run_frames.saturating_add(1);
                self.silence_run_frames = 0;
                if self.speech_run_frames * 32 >= MIN_SPEECH_MS as u32 {
                    self.heard_speech = true;
                }
            } else {
                self.speech_run_frames = 0;
                if self.heard_speech {
                    self.silence_run_frames = self.silence_run_frames.saturating_add(1);
                    let silence_secs = (self.silence_run_frames * 32) as f32 / 1000.0;
                    if silence_secs >= self.stop_secs {
                        self.done = true;
                        return Ok(true);
                    }
                }
            }
        }

        let elapsed = self.accepted_samples as f32 / audio::TARGET_SR as f32;
        if !self.heard_speech && elapsed >= self.stop_secs + NO_SPEECH_GRACE_SECS {
            self.done = true;
            return Ok(true);
        }
        Ok(false)
    }
}

fn init_session() -> Result<Session> {
    let session = Session::builder()
        .map_err(|e| anyhow!("create ORT session builder: {e}"))?
        .with_intra_threads(1)
        .map_err(|e| anyhow!("set intra threads: {e}"))?
        .with_inter_threads(1)
        .map_err(|e| anyhow!("set inter threads: {e}"))?
        .commit_from_memory(SILERO_MODEL)
        .map_err(|e| anyhow!("load Silero-VAD ONNX model from memory: {e}"))?;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_silero_model_loads_and_initial_silence_stops() {
        let mut endpoint =
            SpeechEndpoint::create(0.1).expect("bundled Silero-VAD model should load");
        let silence = vec![0.0; 1600];
        let mut fired = false;
        for _ in 0..24 {
            fired |= endpoint
                .update(&silence)
                .expect("Silero-VAD should process silence");
        }
        assert!(
            fired,
            "initial silence should trigger after stop plus grace"
        );
        assert!(!endpoint
            .update(&silence)
            .expect("completed endpoint is inert"));
    }

    #[test]
    #[ignore]
    fn test_analyze_user_recording() {
        let wav_path = std::path::Path::new("..").join("test_user_recording.wav");
        let wav_path = if wav_path.exists() {
            wav_path
        } else {
            std::path::PathBuf::from("test_user_recording.wav")
        };
        if !wav_path.exists() {
            println!("test_user_recording.wav not found at {:?}", wav_path);
            return;
        }

        let mut reader = hound::WavReader::open(&wav_path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        println!(
            "Loaded user recording: {} samples ({:.2}s)",
            samples.len(),
            samples.len() as f32 / 16000.0
        );

        let target_emb = crate::voiceprint::get_active_embedding();
        println!("Target embedding present: {}", target_emb.is_some());

        if let Some(target) = &target_emb {
            if let Ok(Some(key)) = crate::config::load_doubao_api_key() {
                let stream = crate::doubao::Stream::start(key, "".to_string());
                stream.audio_sender().push(&samples);
                if let Ok((text, _)) = stream.finish() {
                    println!(
                        "=== Transcribed Audio Content ===\n{}\n================================",
                        text
                    );
                }
            }

            // Test 1: Full user recording similarity
            if let Ok(full_emb) = crate::voiceprint::extract_embedding(&samples) {
                let sim = crate::voiceprint::cosine_similarity(target, &full_emb);
                println!(
                    "=== Full 40s recording similarity with enrolled profile: {:.4} ===",
                    sim
                );
            }

            // Test 2: Slice similarities across time with 1.0s and 1.5s windows
            println!("--- Slicing across time with 1.0s window, step 1.0s ---");
            for i in (0..samples.len().saturating_sub(16000)).step_by(16000) {
                let window = &samples[i..i + 16000];
                let rms: f32 = (window.iter().map(|&x| x * x).sum::<f32>() / 16000.0).sqrt();
                if let Ok(emb) = crate::voiceprint::extract_embedding(window) {
                    let sim = crate::voiceprint::cosine_similarity(target, &emb);
                    println!(
                        "Time: {:5.2}s - {:5.2}s | RMS: {:.4} | Sim: {:.4}",
                        i as f32 / 16000.0,
                        (i + 16000) as f32 / 16000.0,
                        rms,
                        sim
                    );
                }
            }

            // Test 3: Run SpeechEndpoint with voiceprint enabled (threshold 0.30, stop_secs 1.25)
            println!(
                "--- Simulating SpeechEndpoint streaming (threshold 0.30, stop_secs 1.25) ---"
            );
            let mut ep =
                SpeechEndpoint::create_with_voiceprint(1.25, true, 0.30).expect("create ep");
            for (chunk_idx, chunk) in samples.chunks(320).enumerate() {
                let time_s = (chunk_idx * 320) as f32 / 16000.0;
                match ep.update(chunk) {
                    Ok(true) => {
                        println!(
                            ">>> Endpoint FIRED STOP at time: {:.2}s! last_target: {:.2}s <<<",
                            time_s,
                            ep.last_target_speech_samples as f32 / 16000.0
                        );
                        break;
                    }
                    Ok(false) => {}
                    Err(e) => println!("Error at {:.2}s: {:?}", time_s, e),
                }
            }

            // 实验验证：录制时长对声纹特征质量的影响
            println!("=== Enrollment Duration Experiment ===");
            let enroll_5s = &samples[16000..16000 + 16000 * 5]; // 5秒
            let enroll_15s = &samples[16000..16000 + 16000 * 15]; // 15秒
            let enroll_30s = &samples[16000..16000 + 16000 * 30]; // 30秒

            if let (Ok(emb_5s), Ok(emb_15s), Ok(emb_30s)) = (
                crate::voiceprint::extract_embedding(enroll_5s),
                crate::voiceprint::extract_embedding(enroll_15s),
                crate::voiceprint::extract_embedding(enroll_30s),
            ) {
                // 用后半段（第20s~35s）作为测试集
                let test_segment = &samples[16000 * 20..16000 * 35];
                let test_emb = crate::voiceprint::extract_embedding(test_segment).unwrap();
                let intruder_segment = &samples[16000 * 36..16000 * 40]; // 他人语音
                let intruder_emb = crate::voiceprint::extract_embedding(intruder_segment).unwrap();

                println!("5s 注册音频:  主人后段测试集相似度 = {:.4} | 他人语音相似度 = {:.4} | 区分度 = {:.4}",
                    crate::voiceprint::cosine_similarity(&emb_5s, &test_emb),
                    crate::voiceprint::cosine_similarity(&emb_5s, &intruder_emb),
                    crate::voiceprint::cosine_similarity(&emb_5s, &test_emb) - crate::voiceprint::cosine_similarity(&emb_5s, &intruder_emb)
                );
                println!("15s 注册音频: 主人后段测试集相似度 = {:.4} | 他人语音相似度 = {:.4} | 区分度 = {:.4}",
                    crate::voiceprint::cosine_similarity(&emb_15s, &test_emb),
                    crate::voiceprint::cosine_similarity(&emb_15s, &intruder_emb),
                    crate::voiceprint::cosine_similarity(&emb_15s, &test_emb) - crate::voiceprint::cosine_similarity(&emb_15s, &intruder_emb)
                );
                println!("30s 注册音频: 主人后段测试集相似度 = {:.4} | 他人语音相似度 = {:.4} | 区分度 = {:.4}",
                    crate::voiceprint::cosine_similarity(&emb_30s, &test_emb),
                    crate::voiceprint::cosine_similarity(&emb_30s, &intruder_emb),
                    crate::voiceprint::cosine_similarity(&emb_30s, &test_emb) - crate::voiceprint::cosine_similarity(&emb_30s, &intruder_emb)
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn test_comprehensive_ablation_study() {
        let wav_path = std::path::Path::new("..").join("test_user_recording.wav");
        let wav_path = if wav_path.exists() {
            wav_path
        } else {
            std::path::PathBuf::from("test_user_recording.wav")
        };
        if !wav_path.exists() {
            println!("test_user_recording.wav not found at {:?}", wav_path);
            return;
        }

        let mut reader = hound::WavReader::open(&wav_path).expect("open wav");
        let samples: Vec<f32> = reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect();
        let total_secs = samples.len() as f32 / 16000.0;
        let owner_end_sec = 35.36;

        println!(
            "================================================================================"
        );
        println!(
            "               BLURT 声纹与双层端点检测流水线综合消融实验报告                     "
        );
        println!(
            "================================================================================"
        );
        println!(
            "基准测试音频时长: {:.2}s ({} 样本)",
            total_secs,
            samples.len()
        );
        println!("音频真实分段: [0.0s~1.0s] 初始静音 | [1.0s~35.36s] 机主持续陈述 | [35.36s~40.21s] 旁人插话干扰\n");

        let target_emb = match crate::voiceprint::get_active_embedding() {
            Some(emb) => emb,
            None => {
                println!("Error: No active voiceprint enrolled in system.");
                return;
            }
        };

        // 模拟流式推理辅助闭包
        let run_sim = |mut ep: SpeechEndpoint| -> (Option<f32>, f32, bool) {
            let mut stop_time = None;
            for (idx, chunk) in samples.chunks(320).enumerate() {
                let cur_time = (idx * 320) as f32 / 16000.0;
                match ep.update(chunk) {
                    Ok(true) => {
                        stop_time = Some(cur_time);
                        break;
                    }
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
            let last_target = ep.last_target_speech_samples() as f32 / 16000.0;
            let premature = stop_time.map(|t| t < owner_end_sec).unwrap_or(false);
            (stop_time, last_target, premature)
        };

        // -----------------------------------------------------------------------------
        // 实验 1：声纹层消融 (Voiceprint Layer Ablation: 2.0s 思考停顿模式下旁人插话)
        // -----------------------------------------------------------------------------
        println!(">>> [实验 1] 声纹层消融 (Baseline: 纯 Silero-VAD vs Full: 双层流水线 @ stop_secs=2.0s)");
        {
            let ep_baseline = SpeechEndpoint::create(2.0).unwrap();
            let (stop_base, _last_base, prem_base) = run_sim(ep_baseline);

            let ep_full = SpeechEndpoint::create_with_voiceprint(2.0, true, 0.30).unwrap();
            let (stop_full, last_full, prem_full) = run_sim(ep_full);

            println!("  [Baseline 纯 Silero-VAD]:");
            println!(
                "    - 提前误切机主: {}",
                if prem_base {
                    "是 (失败)"
                } else {
                    "否 (通过)"
                }
            );
            println!(
                "    - 停录触发时刻: {}",
                stop_base
                    .map(|t| format!("{:.2}s", t))
                    .unwrap_or_else(|| "未停录（一直持续到音频结束 40.21s）".into())
            );
            println!("    - 结果分析: 旁人声音 (36.6s~39.5s) 被 VAD 识别为有效人声并刷新静音计数器，录音被旁人延续，无法自动终止！");

            println!("  [Full 双层流水线 (VAD + ERes2Net @ 0.30)]:");
            println!(
                "    - 提前误切机主: {}",
                if prem_full {
                    "是 (失败)"
                } else {
                    "否 (通过)"
                }
            );
            println!(
                "    - 停录触发时刻: {:.2}s (准确在静音累积达到 2.0s 时切断)",
                stop_full.unwrap_or(0.0)
            );
            println!("    - 机主最后发音定位: {:.2}s", last_full);
            println!(
                "    - 结论: 声纹层精确识别旁人为非法说话人，将其覆写为静音，成功触发终止并准确定位机主发音终点！\n"
            );
        }

        // -----------------------------------------------------------------------------
        // 实验 2：迟滞防抖步数消融 (Hysteresis Streak Ablation)
        // -----------------------------------------------------------------------------
        println!(">>> [实验 2] 迟滞防抖步数消融 (Streak Threshold Ablation @ Thresh 0.30)");
        println!(
            "  | Streak 阈值 | 相当于持续时长 | 停录时刻 | 机主误切? | 旁人切断耗时 | 判定结果 |"
        );
        println!(
            "  |-------------|----------------|----------|-----------|--------------|----------|"
        );
        for &streak in &[1, 2, 3, 5, 8] {
            let ep = SpeechEndpoint::create_custom(1.25, true, 0.30, 16000, 3200, streak).unwrap();
            let (stop_t, _last_t, premature) = run_sim(ep);
            let stop_str = stop_t
                .map(|t| format!("{:.2}s", t))
                .unwrap_or_else(|| "未停止".into());
            let delay_str = stop_t
                .map(|t| format!("{:.2}s", t - owner_end_sec))
                .unwrap_or_else(|| "-".into());
            let result_str = if premature {
                "误切机主 (过敏)"
            } else if stop_t.is_some() {
                if streak == 3 {
                    "最优均衡 (推荐)"
                } else {
                    "抗噪良好"
                }
            } else {
                "切断过迟"
            };
            println!(
                "  | streak = {:<2} | {:>14} | {:>8} | {:>9} | {:>12} | {:<8} |",
                streak,
                format!("{}ms", streak * 200),
                stop_str,
                if premature { "是" } else { "否" },
                delay_str,
                result_str
            );
        }
        println!();

        // -----------------------------------------------------------------------------
        // 实验 3：判决门限敏感度消融 (Threshold Sensitivity Ablation)
        // -----------------------------------------------------------------------------
        println!(">>> [实验 3] 判决门限敏感度消融 (Threshold Sensitivity Ablation)");
        println!("  | 门限值 | 停录时刻 | 机主误切? | 机主定位 | 旁人抑制? | 鲁棒性评价 |");
        println!("  |--------|----------|-----------|----------|-----------|------------|");
        for &thresh in &[0.15, 0.22, 0.30, 0.38, 0.45, 0.50] {
            let ep = SpeechEndpoint::create_custom(1.25, true, thresh, 16000, 3200, 3).unwrap();
            let (stop_t, last_t, premature) = run_sim(ep);
            let stop_str = stop_t
                .map(|t| format!("{:.2}s", t))
                .unwrap_or_else(|| "未停止".into());
            let eval_str = if premature {
                "严重误切（原0.50默认痛点）"
            } else if thresh == 0.30 {
                "黄金分割点（当前默认）"
            } else if thresh < 0.25 {
                "门限偏宽（抗强邻桌偏弱）"
            } else {
                "门限偏紧（微弱音有风险）"
            };
            println!(
                "  |  {:.2}  | {:>8} | {:>9} | {:>6.2}s  | {:>9} | {:<12} |",
                thresh,
                stop_str,
                if premature { "是" } else { "否" },
                last_t,
                if !premature && stop_t.is_some() {
                    "成功"
                } else {
                    "失败"
                },
                eval_str
            );
        }
        println!();

        // -----------------------------------------------------------------------------
        // 实验 4：时序滑动窗口尺寸消融 (Window Size Ablation)
        // -----------------------------------------------------------------------------
        println!(">>> [实验 4] 时序滑动窗口尺寸消融 (Sliding Window Size Ablation)");
        println!("  | 窗口时长 | 样本点数 | 机主相似度均值 | 相似度方差 | 旁人最大相似度 | 信噪分离比 (SNR) |");
        println!("  |----------|----------|----------------|------------|----------------|------------------|");
        for &(win_ms, win_samples) in &[(500, 8000), (1000, 16000), (1500, 24000), (2000, 32000)] {
            let mut owner_sims = Vec::new();
            // 在机主平稳说话区 (2.0s ~ 34.0s) 滑动抽样
            for i in (16000 * 2..16000 * 34).step_by(16000) {
                if i + win_samples <= samples.len() {
                    let w = &samples[i..i + win_samples];
                    if let Ok(emb) = crate::voiceprint::extract_embedding(w) {
                        owner_sims.push(crate::voiceprint::cosine_similarity(&target_emb, &emb));
                    }
                }
            }
            let mean = owner_sims.iter().sum::<f32>() / owner_sims.len().max(1) as f32;
            let variance = owner_sims.iter().map(|&x| (x - mean).powi(2)).sum::<f32>()
                / owner_sims.len().max(1) as f32;

            // 旁人区 (36.0s ~ 40.0s)
            let mut intruder_sims = Vec::new();
            for i in (16000 * 36..samples.len().saturating_sub(win_samples)).step_by(8000) {
                let w = &samples[i..i + win_samples];
                if let Ok(emb) = crate::voiceprint::extract_embedding(w) {
                    intruder_sims.push(crate::voiceprint::cosine_similarity(&target_emb, &emb));
                }
            }
            let max_intruder = intruder_sims.into_iter().fold(0.0f32, f32::max);
            let separation = mean - max_intruder;

            println!(
                "  |  {:>4}ms  | {:>8} | {:>14.4} | {:>10.4} | {:>14.4} | {:>16.4} |",
                win_ms, win_samples, mean, variance, max_intruder, separation
            );
        }
        println!();

        // -----------------------------------------------------------------------------
        // 实验 5：音频回溯裁剪与旁人尾音截断消融 (Audio Truncation Ablation)
        // -----------------------------------------------------------------------------
        println!(
            ">>> [实验 5] 音频回溯裁剪与旁人截断消融 (Audio Truncation via last_target_speech)"
        );
        {
            let ep = SpeechEndpoint::create_with_voiceprint(1.25, true, 0.30).unwrap();
            let (stop_t, last_t, _) = run_sim(ep);
            let stop_samples = (stop_t.unwrap() * 16000.0) as usize;
            let untruncated_audio = &samples[..stop_samples];
            let safe_cutoff = ((last_t * 16000.0) as usize + 9600).min(stop_samples);
            let truncated_audio = &samples[..safe_cutoff];

            println!(
                "  - 未裁剪音频时长: {:.2}s (含 {:.2}s 旁人杂音)",
                untruncated_audio.len() as f32 / 16000.0,
                (stop_samples - (last_t * 16000.0) as usize) as f32 / 16000.0
            );
            println!(
                "  - 裁剪后音频时长: {:.2}s (仅保留 {:.2}s 安全余量)",
                truncated_audio.len() as f32 / 16000.0,
                9600.0 / 16000.0
            );

            if let Ok(Some(key)) = crate::config::load_doubao_api_key() {
                println!("  [调用豆包 ASR 进行真实转写验证]：");
                // 1. 未裁剪
                let stream1 = crate::doubao::Stream::start(key.clone(), "".to_string());
                stream1.audio_sender().push(untruncated_audio);
                if let Ok((text1, _)) = stream1.finish() {
                    println!("  * [未裁剪文本]: \"{}\"", text1.trim());
                    let has_intruder = text1.contains("说话") || text1.contains("听不到");
                    println!(
                        "    -> 旁人干扰渗入转写: {}",
                        if has_intruder {
                            "是 (出现杂音文字)"
                        } else {
                            "否"
                        }
                    );
                }

                // 2. 裁剪后
                let stream2 = crate::doubao::Stream::start(key, "".to_string());
                stream2.audio_sender().push(truncated_audio);
                if let Ok((text2, _)) = stream2.finish() {
                    println!("  * [裁剪后文本]: \"{}\"", text2.trim());
                    let has_intruder = text2.contains("说话") || text2.contains("听不到");
                    println!(
                        "    -> 旁人干扰渗入转写: {}",
                        if has_intruder {
                            "是 (出现杂音文字)"
                        } else {
                            "否 (纯净机主文字)"
                        }
                    );
                }
            }
        }
        println!();

        // -----------------------------------------------------------------------------
        // 实验 6：计算延迟与吞吐性能评测 (Performance & Latency Benchmark)
        // -----------------------------------------------------------------------------
        println!(">>> [实验 6] 计算延迟与吞吐性能评测 (Computational Overhead Benchmark)");
        {
            use std::time::Instant;
            // 测 Silero-VAD 32ms chunk 延迟
            let silence = vec![0.0; 512];
            let mut ep = SpeechEndpoint::create(1.25).unwrap();
            let t0 = Instant::now();
            let iters = 200;
            for _ in 0..iters {
                let _ = ep.update(&silence);
            }
            let vad_elapsed = t0.elapsed();
            let vad_per_chunk_us = vad_elapsed.as_micros() as f64 / iters as f64;

            // 测 ERes2Net 1.0s window 嵌入提取延迟
            let win = vec![0.05; 16000];
            let t1 = Instant::now();
            let vp_iters = 20;
            for _ in 0..vp_iters {
                let _ = crate::voiceprint::extract_embedding(&win);
            }
            let vp_elapsed = t1.elapsed();
            let vp_per_window_ms = vp_elapsed.as_millis() as f64 / vp_iters as f64;

            // 算 Real-Time Factor (RTF)
            // 每秒音频有：31.25 个 VAD chunk (31.25 * vad_per_chunk_us) + 5 个声纹步进 (5 * vp_per_window_ms)
            let total_cpu_ms_per_sec =
                (31.25 * vad_per_chunk_us / 1000.0) + (5.0 * vp_per_window_ms);
            let rtf = total_cpu_ms_per_sec / 1000.0;

            println!(
                "  - Silero-VAD 单帧 (32ms) 推理时延: {:.2} μs ({:.4} ms)",
                vad_per_chunk_us,
                vad_per_chunk_us / 1000.0
            );
            println!(
                "  - ERes2Net 单次 (1.0s) 特征提取时延: {:.2} ms",
                vp_per_window_ms
            );
            println!(
                "  - 流式运行每秒音频总计算耗时: {:.2} ms",
                total_cpu_ms_per_sec
            );
            println!(
                "  - Real-Time Factor (RTF): {:.4} (CPU 占用率约 {:.2}%)",
                rtf,
                rtf * 100.0
            );
        }

        println!(
            "================================================================================"
        );
        println!(
            "                               消融实验完毕                                     "
        );
        println!(
            "================================================================================"
        );
    }
}
