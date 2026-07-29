//! Streaming FSMN voice endpoint detection for toggle-mode recordings.

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use vad_burn::{FsmnVadModel, FsmnVadStream, VadOptions};

use crate::audio;

const FSMN_MODEL: &[u8] = include_bytes!("../fsmn_vad_model.pt");
const FSMN_CMVN: &[u8] = include_bytes!("../fsmn_vad_am.mvn");
const FSMN_LICENSE: &str = include_str!("../FSMN_VAD_LICENSE.txt");
const SPEECH_NOISE_THRESHOLD: f32 = 0.6;
const MIN_SPEECH_MS: u64 = 100;
const FRAME_MS: u32 = 10;
const NO_SPEECH_GRACE_SECS: f32 = 2.0;

static BUNDLED_MODEL: OnceLock<Result<Mutex<FsmnVadModel>, String>> = OnceLock::new();

/// Stateful neural VAD plus Blurt's initial no-speech timeout.
pub struct SpeechEndpoint {
    stream: FsmnVadStream,
    stop_secs: f32,
    accepted_samples: u64,
    speech_run_frames: u32,
    heard_speech: bool,
    done: bool,
}

impl SpeechEndpoint {
    pub fn create(stop_secs: f32) -> Result<Self> {
        let model = BUNDLED_MODEL.get_or_init(|| {
            ensure_bundled_model()
                .and_then(FsmnVadModel::from_pretrained)
                .map(Mutex::new)
                .map_err(|error| format!("{error:#}"))
        });
        let model = model
            .as_ref()
            .map_err(|error| anyhow!("FSMN-VAD initialization failed: {error}"))?;
        Ok(Self::from_model(stop_secs, &model.lock()))
    }

    fn from_model(stop_secs: f32, model: &FsmnVadModel) -> Self {
        let options = VadOptions {
            threshold: SPEECH_NOISE_THRESHOLD,
            min_speech_ms: MIN_SPEECH_MS,
            min_silence_ms: (stop_secs * 1000.0).round().max(1.0) as u64,
            max_segment_ms: 0,
            pad_ms: 0,
        };
        Self {
            stream: model.new_stream(options),
            stop_secs,
            accepted_samples: 0,
            speech_run_frames: 0,
            heard_speech: false,
            done: false,
        }
    }

    /// Accept incremental 16 kHz mono samples. Returns true exactly once after
    /// FSMN confirms a speech segment ended, or after the initial no-speech timeout.
    pub fn update(&mut self, samples: &[f32]) -> Result<bool> {
        if samples.is_empty() || self.done {
            return Ok(false);
        }

        let previous_frames = self.stream.frame_scores().len();
        let completed_segments = self
            .stream
            .push(samples, audio::TARGET_SR)
            .context("FSMN-VAD streaming inference failed")?;
        self.accepted_samples = self.accepted_samples.saturating_add(samples.len() as u64);

        for scores in &self.stream.frame_scores()[previous_frames..] {
            if is_speech_frame(scores) {
                self.speech_run_frames = self.speech_run_frames.saturating_add(1);
                if self.speech_run_frames * FRAME_MS >= MIN_SPEECH_MS as u32 {
                    self.heard_speech = true;
                }
            } else {
                self.speech_run_frames = 0;
            }
        }

        if !completed_segments.is_empty() {
            self.done = true;
            return Ok(true);
        }

        let elapsed = self.accepted_samples as f32 / audio::TARGET_SR as f32;
        if !self.heard_speech && elapsed >= self.stop_secs + NO_SPEECH_GRACE_SECS {
            self.done = true;
            return Ok(true);
        }
        Ok(false)
    }
}

fn is_speech_frame(scores: &[f32]) -> bool {
    let silence = scores.first().copied().unwrap_or(1.0).clamp(0.0, 1.0);
    1.0 - silence >= silence + SPEECH_NOISE_THRESHOLD
}

fn ensure_bundled_model() -> Result<PathBuf> {
    #[cfg(not(test))]
    let root = crate::config::app_dir().join("vad");
    #[cfg(test)]
    let root = std::env::temp_dir().join("blurt-tests").join("vad");
    fs::create_dir_all(&root)
        .with_context(|| format!("create FSMN-VAD model directory: {}", root.display()))?;
    write_if_stale(&root.join("model.pt"), FSMN_MODEL)?;
    write_if_stale(&root.join("am.mvn"), FSMN_CMVN)?;

    let license = root.join("LICENSE.txt");
    if !license.is_file() {
        fs::write(&license, FSMN_LICENSE)
            .with_context(|| format!("write FSMN-VAD license: {}", license.display()))?;
    }
    Ok(root)
}

fn write_if_stale(path: &Path, bytes: &[u8]) -> Result<()> {
    let current = fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() == bytes.len() as u64)
        .unwrap_or(false);
    if !current {
        fs::write(path, bytes)
            .with_context(|| format!("write bundled model: {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_classifier_matches_funasr_score_rule() {
        assert!(is_speech_frame(&[0.1]));
        assert!(is_speech_frame(&[0.2]));
        assert!(!is_speech_frame(&[0.21]));
        assert!(!is_speech_frame(&[0.9]));
    }

    #[test]
    fn bundled_fsmn_model_loads_and_initial_silence_stops() {
        let mut endpoint = SpeechEndpoint::create(0.1).expect("bundled FSMN-VAD model should load");
        let silence = vec![0.0; 1600];
        let mut fired = false;
        for _ in 0..24 {
            fired |= endpoint
                .update(&silence)
                .expect("FSMN-VAD should process silence");
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
