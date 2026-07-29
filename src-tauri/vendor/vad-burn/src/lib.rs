//! Locally vendored FSMN portion of vad-burn 0.1.3.
//!
//! The feature frontend uses the pure Rust `kaldi-native-fbank` crate so the
//! Windows binary can consistently link against the static C runtime.

pub mod fsmn;
mod types;

pub use fsmn::{
    FeatureTensor, FsmnForwardTiming, FsmnVadDetection, FsmnVadModel, FsmnVadStream, FsmnVadTiming,
};
pub use types::{DurationMs, TimeRange, VadOptions, VadSegment, Waveform};
