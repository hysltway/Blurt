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

static BUNDLED_SESSION: OnceLock<Result<Mutex<Session>, String>> = OnceLock::new();

/// Stateful neural Silero VAD endpoint detector.
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
}

impl SpeechEndpoint {
    pub fn create(stop_secs: f32) -> Result<Self> {
        let session_lock = BUNDLED_SESSION.get_or_init(|| {
            init_session()
                .map(Mutex::new)
                .map_err(|error| format!("{error:#}"))
        });
        let _ = session_lock
            .as_ref()
            .map_err(|error| anyhow!("Silero-VAD initialization failed: {error}"))?;

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
        })
    }

    /// Accept incremental 16 kHz mono samples. Returns true exactly once after
    /// Silero confirms a speech segment ended, or after the initial no-speech timeout.
    pub fn update(&mut self, samples: &[f32]) -> Result<bool> {
        if samples.is_empty() || self.done {
            return Ok(false);
        }

        self.accepted_samples = self.accepted_samples.saturating_add(samples.len() as u64);
        self.buffer.extend_from_slice(samples);

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

            let is_speech = prob >= SPEECH_THRESHOLD;
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
}
