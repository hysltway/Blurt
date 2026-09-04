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
    vp_step_samples: usize,
    non_target_streak: usize,
    intruder_active: bool,
    last_target_speech_samples: usize,
}

impl SpeechEndpoint {
    #[allow(dead_code)]
    pub fn create(stop_secs: f32) -> Result<Self> {
        Self::create_with_voiceprint(stop_secs, false, 0.30)
    }

    pub fn create_with_voiceprint(
        stop_secs: f32,
        voiceprint_enabled: bool,
        voiceprint_threshold: f32,
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
            vp_window: Vec::with_capacity(VP_WINDOW_SAMPLES + CHUNK_SAMPLES),
            vp_step_samples: 0,
            non_target_streak: 0,
            intruder_active: false,
            last_target_speech_samples: 0,
        })
    }

    pub fn last_target_speech_samples(&self) -> usize {
        self.last_target_speech_samples
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
            if self.vp_window.len() > VP_WINDOW_SAMPLES {
                let excess = self.vp_window.len() - VP_WINDOW_SAMPLES;
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
                    // 当累积步长达到步进门限且窗口具备足够样本时（>= 8000 样本，0.5s~1.0s），执行声纹嵌入比对
                    if self.vp_step_samples >= VP_STEP_SAMPLES && self.vp_window.len() >= 8000 {
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
                                // 需连续 3 步（>= 600ms）持续未命中，才判定为旁人持续说话干扰
                                if self.non_target_streak >= 3 {
                                    self.intruder_active = true;
                                }
                            }
                        }
                    }

                    // 若已被判定为旁人持续插话，则将语音判定覆写为静音，允许静音超时正常断句
                    if self.intruder_active {
                        is_speech = false;
                    } else {
                        // 主人正常发音中，持续刷新主人有效样本偏移
                        self.last_target_speech_samples = self.accepted_samples as usize;
                    }
                } else if self.non_target_streak > 0 {
                    // 自然静音帧：重置非主人计数，以便后续主人开口时能平滑捕获
                    self.non_target_streak = 0;
                    self.intruder_active = false;
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
}
